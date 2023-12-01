use anyhow::Context as _;
use http::Response;
use http_body_util::BodyExt;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use wasmtime::{
    component::{Component, Linker, Resource},
    Config, Engine, Store,
};
use wasmtime_wasi::preview2::{Table, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::{
    proxy::{
        exports::wasi::http::incoming_handler::{IncomingRequest, ResponseOutparam},
        Proxy,
    },
    types::{default_send_request, IncomingResponseInternal},
    WasiHttpCtx, WasiHttpView,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let mut args = std::env::args().skip(1);
    let middleware_path = args
        .next()
        .context("missing arg with path to middleware component")?;
    let business_path = args
        .next()
        .context("missing arg with path to business logic component")?;

    // Configure the context
    let mut context = Context::new();
    let request = context.create_request("Hello, world!")?;
    let (out, response) = context.create_response()?;

    let runtime = Runtime::new()?;
    // Configure the middleware
    let mut middleware_store = runtime.store(context);
    let middleware_proxy = runtime
        .proxy(&mut middleware_store, &std::fs::read(middleware_path)?)
        .await?;

    // Configure the business logic
    let mut business_store = runtime.store(Context::new());
    let business_proxy = runtime
        .proxy(&mut business_store, &std::fs::read(business_path)?)
        .await?;
    middleware_store.data_mut().proxy =
        Some(Arc::new(Mutex::new((business_store, business_proxy))));

    // Make the actual request
    middleware_proxy
        .wasi_http_incoming_handler()
        .call_handle(&mut middleware_store, request, out)
        .await?;

    // Get the response
    let response = response.await?;
    let body = response.body().to_vec();
    println!("{body:x?}");
    Ok(())
}

struct Runtime {
    engine: Engine,
    linker: Linker<Context>,
}

impl Runtime {
    fn new() -> anyhow::Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine = Engine::new(&config)?;

        let mut linker = Linker::<Context>::new(&engine);
        linker.allow_shadowing(true);
        wasmtime_wasi::preview2::command::sync::add_to_linker(&mut linker)?;
        wasmtime_wasi_http::proxy::add_to_linker(&mut linker)?;
        Ok(Self { engine, linker })
    }

    fn store(&self, context: Context) -> Store<Context> {
        Store::new(&self.engine, context)
    }

    async fn proxy(&self, store: &mut Store<Context>, component: &[u8]) -> anyhow::Result<Proxy> {
        let instance = self
            .linker
            .instantiate_async(&mut *store, &Component::new(&self.engine, component)?)
            .await
            .unwrap();
        Proxy::new(store, &instance)
    }
}

struct Context {
    table: Table,
    ctx: WasiCtx,
    http: WasiHttpCtx,
    proxy: Option<Arc<Mutex<(Store<Self>, Proxy)>>>,
}

impl Context {
    fn new() -> Self {
        let table = Table::new();
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdout().inherit_stderr();
        let ctx = builder.build();
        let http = WasiHttpCtx;
        Context {
            table,
            ctx,
            http,
            proxy: None,
        }
    }

    fn create_request(
        &mut self,
        body: impl Into<String>,
    ) -> anyhow::Result<Resource<IncomingRequest>> {
        let body = http_body_util::combinators::BoxBody::new(body.into())
            .map_err(|_| wasmtime_wasi_http::bindings::http::types::ErrorCode::InternalError(None))
            .boxed();
        let req = http::request::Builder::new()
            .uri("http://localhost:3000/")
            .body(body)?;

        self.new_incoming_request(req)
    }

    fn create_response(
        &mut self,
    ) -> anyhow::Result<(
        Resource<ResponseOutparam>,
        impl std::future::Future<Output = anyhow::Result<Response<bytes::Bytes>>>,
    )> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        Ok((self.new_response_outparam(tx)?, async {
            let response = rx.await;
            let response = response??;
            let (parts, body) = response.into_parts();
            let body = body
                .collect()
                .await
                .map(|c| c.to_bytes())
                .map_err(|e| anyhow::anyhow!(e))?;
            Ok(Response::from_parts(parts, body))
        }))
    }
}

impl WasiView for Context {
    fn table(&self) -> &Table {
        &self.table
    }

    fn table_mut(&mut self) -> &mut Table {
        &mut self.table
    }

    fn ctx(&self) -> &WasiCtx {
        &self.ctx
    }

    fn ctx_mut(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }
}

impl WasiHttpView for Context {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }

    fn table(&mut self) -> &mut Table {
        &mut self.table
    }

    fn send_request(
        &mut self,
        request: wasmtime_wasi_http::types::OutgoingRequest,
    ) -> wasmtime::Result<
        wasmtime::component::Resource<wasmtime_wasi_http::types::HostFutureIncomingResponse>,
    >
    where
        Self: Sized,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let Some(proxy) = self.proxy.clone() else {
            return default_send_request(self, request);
        };
        let worker = Arc::new(wasmtime_wasi::preview2::spawn(async move {
            let (store, proxy) = &mut *proxy.lock().await;
            let ctx = store.data_mut();
            let response = ctx.new_response_outparam(tx).unwrap();
            let req = ctx.new_incoming_request(request.request).unwrap();
            proxy
                .wasi_http_incoming_handler()
                .call_handle(store, req, response)
                .await
                .unwrap();
        }));
        let handle = wasmtime_wasi::preview2::spawn(async move {
            let resp = rx.await??;
            let resp = IncomingResponseInternal {
                resp,
                worker,
                between_bytes_timeout: Duration::from_secs(10),
            };
            Ok(Ok(resp))
        });

        Ok(self
            .table()
            .push(wasmtime_wasi_http::types::HostFutureIncomingResponse::new(
                handle,
            ))?)
    }
}
