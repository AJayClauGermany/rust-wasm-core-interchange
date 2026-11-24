# rust-wasm-core-interchange

The framework-agnostic core for running **hot-reloadable, capability-restricted
WebAssembly modules** inside a Rust host, plus a set of guest modules in
different languages that exercise it.

Rust is the host language. Guests can be written in any language that compiles
to a [WIT component](https://component-model.bytecodealliance.org/) or to an
[Extism](https://extism.org) plugin, and the same module runs unchanged in
every host built on this core:

- [`tauri-plugin-wasm-backend`](https://github.com/AJayClauGermany/tauri-plugin-wasm-backend):
  Tauri apps (IPC commands, permissions and scopes on top of this crate).
- Any other Rust app (GPUI and its derivatives, CLIs, servers, tests) by
  implementing the `Platform` trait.

## Layout

| Path                                                         | What                                                                        |
|--------------------------------------------------------------|-----------------------------------------------------------------------------|
| [`crates/wasm-backend-core`](crates/wasm-backend-core)       | The core: engines, capabilities, module registry, hot reload, `Platform`.   |
| [`crates/wasm-backend-core/wit`](crates/wasm-backend-core/wit) | The guest-facing WIT package (`tauri:backend`), also exported as `WIT`.   |
| [`guests/`](guests)                                          | Guest modules by language, built and run by the integration tests.          |

## Using the core

```toml
[dependencies]
wasm-backend-core = { git = "https://github.com/AJayClauGermany/rust-wasm-core-interchange" }
```

Cargo features (all but `specta` are on by default): `component` (WIT engine,
wasmtime), `extism` (Extism engine), `hot-reload` (file watching), `specta`
(derive `specta::Type` on the public models).

A host implements `Platform`: how events are delivered, where the standard
directories (`$APPDATA`, `$RESOURCE`, …) are, and where blocking work runs.
Apps without an event system can delegate to the bundled `EventBus`.

```rust
use wasm_backend_core::{Backend, Capabilities, ModuleConfig};

let backend = Backend::builder(MyPlatform::new())
    .host_command("now", |_call, _args: ()| Ok::<_, String>(42))
    .build()?;
backend.load(
    "greeter",
    ModuleConfig::from_path("greeter.wasm")
        .capabilities(Capabilities::default().allow_host_command("now")),
)?;
let out = backend.invoke("greeter", "greet", serde_json::json!({ "name": "GPUI" }))?;
```

`Backend::invoke_async` runs the call through `Platform::spawn_blocking` and
returns a plain future, so it works with any async runtime. See the crate docs
for a complete `Platform` implementation, and
[`tests/common`](crates/wasm-backend-core/tests/common/mod.rs) for a minimal
one.

### Running code you didn't write

A few things the runtime does around a module's own code, for hosts that load
third-party plugins:

- **A load gate.** `BackendBuilder::load_gate` is asked before any module is
  loaded or reloaded, with its name, path, capabilities and the SHA-256 of
  its bytes (`LoadRequest`), before a byte of it is compiled. Returning
  `Err(reason)` refuses the load. `ModuleInfo::sha256` carries the same hash
  afterwards.
- **Declared imports.** `ModuleInfo::requires` lists every interface a WIT
  module imports: all it can reach outside its own memory.
- **Restart limits.** A call that traps (a panic, a timeout, running out of
  memory) resets the module's memory and emits
  `wasm-backend://module-trapped`. With `Capabilities::max_restarts(n)`, the
  trap after the n-th restart stops the module: calls fail with
  `Error::Stopped` until it is reloaded. `ModuleInfo::traps` and `stopped`
  say where it stands.
- **WIT guests without WASI.** A core module built against a WIT world (a
  Rust crate using `wit-bindgen`, built for `wasm32-unknown-unknown`) is
  detected by its `component-type` section and wrapped into a component when
  it loads. It imports no `wasi:*` interface at all, so `tauri:backend/host`
  and the host commands it imports are everything it can reach; see
  [`guests/rust/bare-guest`](guests/rust/bare-guest).

The guest API (the `tauri:backend` WIT package and the `tauri_*` Extism host
functions) is the same on every host. Access control for *callers* of
`Backend::invoke` (e.g. Tauri's `invoke` scopes) is up to each host;
`ModuleInfo::exposed` carries the module's setting.

## Guests and tests

Every guest in [`guests/`](guests) is built and run by the core's integration
tests, so a change to the core is checked against each guest language:

```sh
rustup target add wasm32-wasip2 wasm32-unknown-unknown   # Rust guests
(cd guests/javascript && npm ci)                          # JavaScript guests
cargo test --workspace
```

Without `npm ci`, the JavaScript tests are skipped; set
`WASM_CORE_REQUIRE_ALL_GUESTS=1` (as CI does) to make that an error instead.
[`guests/README.md`](guests/README.md) explains how to add a language.

## Wasmtime compile times

Wasmtime compiles WASM to native code when a module loads, which is slow in an
unoptimized build (seconds for a JavaScript module). Hosts should optimize the
compiler crates in dev builds; see the `[profile.dev.package.*]` sections of
the Tauri plugin's example app.

## License

The repository is licensed under the [Boost Software License 1.0](LICENSE).
The `wasm-backend-core` crate keeps the license it was published under in
`tauri-plugin-wasm-backend`, the
[Apache License 2.0 with LLVM Exceptions](crates/wasm-backend-core/LICENSE)
(`Apache-2.0 WITH LLVM-exception`).
