use std::{sync::Arc, time::Duration};

use http_body_util::BodyExt;
use tokio::sync::Mutex;
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};
use wasmtime_wasi::preview2::{Table, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::{
    proxy::Proxy,
    types::{default_send_request, IncomingResponseInternal},
    WasiHttpCtx, WasiHttpView,
};

#[tokio::main]
async fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let component_path = args.next().expect("missing arg with path to component");
    let business_path = args.next().expect("missing arg with path to component");
    let middleware_bytes = std::fs::read(component_path).unwrap();
    let business_bytes = std::fs::read(business_path).unwrap();
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);

    let engine = Engine::new(&config).unwrap();
    let middleware = Component::new(&engine, &middleware_bytes).unwrap();
    let business = Component::new(&engine, &business_bytes).unwrap();
    let mut linker = Linker::<Context>::new(&engine);
    linker.allow_shadowing(true);
    wasmtime_wasi::preview2::command::sync::add_to_linker(&mut linker).unwrap();
    wasmtime_wasi_http::proxy::add_to_linker(&mut linker).unwrap();

    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut context = Context::new();
    let response = context.new_response_outparam(tx).unwrap();
    let body = http_body_util::combinators::BoxBody::new(String::from("Incoming body"))
        .map_err(|_| wasmtime_wasi_http::bindings::http::types::ErrorCode::InternalError(None))
        .boxed();
    let req = http::request::Builder::new()
        .uri("http://localhost:3000/")
        .body(body)
        .expect("TODO");
    let request = context.new_incoming_request(req).unwrap();
    let mut middleware_store = Store::new(&engine, context);
    let middleware_instance = linker
        .instantiate_async(&mut middleware_store, &middleware)
        .await
        .unwrap();
    let middleware_proxy = Proxy::new(&mut middleware_store, &middleware_instance).unwrap();

    let mut other_store = Store::new(&engine, Context::new());
    let business_instance = linker
        .instantiate_async(&mut other_store, &business)
        .await
        .unwrap();
    let proxy = Proxy::new(&mut other_store, &business_instance).unwrap();
    middleware_store.data_mut().proxy = Some(Arc::new(Mutex::new((other_store, proxy))));
    middleware_proxy
        .wasi_http_incoming_handler()
        .call_handle(&mut middleware_store, request, response)
        .await
        .unwrap();
    let response = rx.await;
    let mut response = response.unwrap().unwrap();
    let mut body = Vec::<u8>::new();
    while let Some(f) = response.body_mut().frame().await {
        if let Some(d) = f.unwrap().data_ref() {
            body.extend(d)
        }
    }
    println!("{body:#x?}");
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
