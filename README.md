# Middleware Demo

Shows two components being used in a middleware chain. One component is the core business logic while the other is a middleware that uses the wasi-http outgoing-handler to call the business logic component.

## Running

To run the demo you must first build the two components using `cargo component`:

```bash
cargo component b -p middleware && cargo component b -p business-logic
```

Then you can run the demo:

```bash
cargo r ./target/wasm32-wasi/debug/middleware.wasm ./target/wasm32-wasi/debug/business_logic.wasm
```