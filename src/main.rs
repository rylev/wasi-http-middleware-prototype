use http_body_util::BodyExt;
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};
use wasmtime_wasi::preview2::{Table, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::{proxy::Proxy, types::default_send_request, WasiHttpCtx, WasiHttpView};

#[tokio::main]
async fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let component_path = args.next().expect("missing arg with path to component");
    let component_bytes = std::fs::read(component_path).unwrap();
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);

    let engine = Engine::new(&config).unwrap();
    let component = Component::new(&engine, &component_bytes).unwrap();
    let mut linker = Linker::<Context>::new(&engine);
    linker.allow_shadowing(true);
    wasmtime_wasi::preview2::command::sync::add_to_linker(&mut linker).unwrap();
    wasmtime_wasi_http::proxy::add_to_linker(&mut linker).unwrap();

    let pre = linker.instantiate_pre(&component).unwrap();
    let table = Table::new();
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdout().inherit_stderr();
    let ctx = builder.build();
    let http = WasiHttpCtx;
    let mut context = Context { table, ctx, http };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let response = context.new_response_outparam(tx).unwrap();
    let body = http_body_util::combinators::BoxBody::new(String::new())
        .map_err(|_| wasmtime_wasi_http::bindings::http::types::ErrorCode::InternalError(None))
        .boxed();
    let req = http::request::Builder::new()
        .uri("http://localhost:3000/")
        .body(body)
        .expect("TODO");
    let request = context.new_incoming_request(req).unwrap();
    let mut store = Store::new(&engine, context);
    let instance = pre.instantiate_async(&mut store).await.unwrap();
    let proxy = Proxy::new(&mut store, &instance).unwrap();
    proxy
        .wasi_http_incoming_handler()
        .call_handle(&mut store, request, response)
        .await
        .unwrap();
    let response = rx.await;
    println!("{response:?}");
}

struct Context {
    table: Table,
    ctx: WasiCtx,
    http: WasiHttpCtx,
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
        println!("THE AUTHORITY! {:?}", request.authority);
        default_send_request(self, request)
    }
}
