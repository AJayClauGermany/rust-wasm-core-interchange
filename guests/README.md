# Guests

Guest modules the core's integration tests build and run, grouped by language:
`guests/<language>/<guest>`.

| Guest                                         | Language   | Engine         | Covers                                                    |
|-----------------------------------------------|------------|----------------|-----------------------------------------------------------|
| [`rust/component-guest`](rust/component-guest) | Rust       | WIT component  | Every part of the host API, state, traps, events          |
| [`rust/typed-guest`](rust/typed-guest)         | Rust       | WIT component  | Host commands imported as typed functions                 |
| [`rust/bare-guest`](rust/bare-guest)           | Rust       | WIT component  | Built for `wasm32-unknown-unknown`: no WASI, wrapped at load |
| [`rust/extism-guest`](rust/extism-guest)       | Rust       | Extism         | The `tauri_*` host functions, JSON commands               |
| [`javascript/greeter`](javascript/greeter)     | JavaScript | WIT component  | componentize-qjs output: records, `result`, events, state |

WIT guests include the host's package with
`include tauri:backend/module@0.1.0;` (or `module-with-events`) in their world.
The test harness writes it to `wit/deps/tauri-backend/backend.wit` before each
build (from `wasm_backend_core::WIT`), so it is never checked in.

## Adding a language

1. Put the guest in `guests/<language>/<name>`, with a `wit/` folder for a WIT
   component.
2. Add a build function next to `rust_guest` / `js_component` in
   [`crates/wasm-backend-core/tests/common/mod.rs`](../crates/wasm-backend-core/tests/common/mod.rs)
   that writes the `.wasm` under `target/guests/` and returns its path. If the
   toolchain may be missing locally, return `None` to skip unless
   `WASM_CORE_REQUIRE_ALL_GUESTS` is set.
3. Add a test file in `crates/wasm-backend-core/tests/` that loads the module
   on the `TestPlatform` and checks its commands.
4. Install the toolchain in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml).
