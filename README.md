# rust-wasm-core-interchange

The framework-agnostic core for running **hot-reloadable, capability-restricted
WebAssembly modules** inside a Rust host, plus a set of guest modules in
different languages that exercise it.

The core is a Rust library. Guests can be written in any language that compiles
to a [WIT component](https://component-model.bytecodealliance.org/) or to an
[Extism](https://extism.org) plugin, and the same module runs unchanged in
every host built on this core:

- [`tauri-plugin-wasm-backend`](https://github.com/AJayClauGermany/tauri-plugin-wasm-backend):
  Tauri apps (IPC commands, permissions and scopes on top of this crate).
- Any other Rust app (GPUI and its derivatives, CLIs, servers, tests) by
  implementing the `Platform` trait.
- Apps in other languages (C and C++, Python, Go, Swift, C#, …) through the
  [C interface](#hosts-in-other-languages).

## Layout

| Path                                                         | What                                                                        |
|--------------------------------------------------------------|-----------------------------------------------------------------------------|
| [`crates/wasm-backend-core`](crates/wasm-backend-core)       | The core: engines, capabilities, module registry, hot reload, `Platform`.   |
| [`crates/wasm-backend-core/wit`](crates/wasm-backend-core/wit) | The guest-facing WIT package (`tauri:backend`), also exported as `WIT`.   |
| [`crates/wasm-backend-ffi`](crates/wasm-backend-ffi)         | The core behind a C interface: `libwasm_backend` and `wasm_backend.h`.      |
| [`guests/`](guests)                                          | Guest modules by language, built and run by the integration tests.          |
| [`hosts/`](hosts)                                            | Hosts by language, built and run against the C interface by its tests.      |

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

### Deciding what a module may do

Capabilities are deny-by-default and set per module. Three things make them
quick to write without giving up precision:

- **Wildcards and deny entries.** In `hostCommands`, `events.emit` and
  `state`, `*` matches anything and an entry starting with `!` takes back
  what the others grant: `["db_*", "!db_drop"]`. A `!` entry always wins.
- **Limits for everything.** `BackendBuilder::limits` (`limits` in `Config`)
  sets the memory, time-per-call and restart limits of every module that
  doesn't set its own, external plugins and modules loaded at runtime
  included.
- **Builders for all of it.**

  ```rust
  Capabilities::default()
      .allow_host_command("db_*")
      .deny_host_command("db_drop")
      .allow_emit("notes://*")
      .allow_state("*")
      .preopen(Preopen::new("$APPDATA/notes", "/data").read_only(true))
      .timeout_ms(1000)
  ```

### Deciding what runs

- **Switching modules off.** `enabled: false` in a module's configuration
  keeps it from loading; `Backend::disable` and `Backend::enable` do the
  same at runtime, for external plugins too.
- **Choosing external plugins.** `ExternalConfig` takes `allow` and `deny`
  lists of plugin names, and `plugins` for what one plugin gets instead of
  the capabilities shared by all:

  ```rust
  ExternalConfig::new("$APPDATA/plugins")
      .capabilities(Capabilities::default().allow_emit("plugin://*"))
      .plugin("weather", ExternalPluginConfig::default()
          .capabilities(Capabilities::default().allow_host("api.weather.example")))
      .deny("legacy-*")
  ```
- **Pinning.** `sha256` in a module's (or plugin's) configuration only loads
  those exact bytes.
- **Seeing the rest.** `Backend::modules` lists what runs.
  `Backend::inactive` lists everything else the backend knows of, each with
  a reason (`Disabled`, `Denied`, `Failed`), the message, its hash and the
  capabilities it would get: what a plugin manager needs to show.

### Running code you didn't write

A few things the runtime does around a module's own code, for hosts that load
third-party plugins:

- **A load gate.** `BackendBuilder::load_gate` is asked before any module is
  loaded or reloaded, with its name, path, capabilities and the SHA-256 of
  its bytes (`LoadRequest`), before a byte of it is compiled. Returning
  `Err(reason)` refuses the load. `ModuleInfo::sha256` carries the same hash
  afterwards. A refused module is listed by `Backend::inactive`; once the
  gate would let it through (the user approved it, say), `Backend::enable`
  loads it.
- **Declared imports.** `ModuleInfo::requires` lists every interface a WIT
  module imports: all it can reach outside its own memory.
- **Restart limits.** A call that traps (a panic, a timeout, running out of
  memory) resets the module's memory and emits
  `wasm-backend://module-trapped`. With `Capabilities::max_restarts(n)`, the
  trap after the n-th restart stops the module: calls fail with
  `Error::Stopped` until it is reloaded. `ModuleInfo::traps` and `stopped`
  say where it stands.
- **A state store per module.** A module's memory is reset when it is
  reloaded or traps. What it stores through the `tauri:backend/state`
  interface (`tauri_state_*` for Extism) is kept by the host instead, behind
  the `state` capability (key patterns, e.g. `Capabilities::allow_state("*")`).
  Writes made during a call are kept when it returns and dropped if it traps.
  `BackendBuilder::state_store(FileStateStore::new(dir))` keeps state between
  runs of the app (or implement `StateStore`); `Backend::state` and
  `clear_state` read and wipe a module's store.
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

## Hosts in other languages

[`wasm-backend-ffi`](crates/wasm-backend-ffi) puts the core behind a C
interface, for apps that aren't written in Rust. It builds `libwasm_backend`
(shared and static), declared in
[`wasm_backend.h`](crates/wasm-backend-ffi/include/wasm_backend.h).

```sh
cargo build --release -p wasm-backend-ffi   # target/release/libwasm_backend.{so,dylib,a}, wasm_backend.dll
```

```c
#include "wasm_backend.h"

static void now(void *user_data, const char *module, const char *args_json, WasmBackendReply *reply) {
  wasm_backend_reply_ok(reply, "1764201600");
}

WasmBackendBuilder *builder;
WasmBackend *backend;
char *out;
wasm_backend_builder_new(
    "{\"modules\": {\"greeter\": {\"path\": \"greeter.wasm\","
    "  \"capabilities\": {\"hostCommands\": [\"now\"]}}}}", &builder, &out);
wasm_backend_builder_host_command(builder, "now", now, NULL, &out);
wasm_backend_builder_build(builder, &backend, &out);

if (wasm_backend_invoke(backend, "greeter", "greet", "{\"name\": \"C\"}", &out) == WASM_BACKEND_STATUS_OK)
  puts(out);                      /* "Hello, C!" */
wasm_backend_string_free(out);
wasm_backend_free(backend);
```

The interface is small on purpose, so that a binding is an afternoon's work:

- Strings are UTF-8 and everything structured is JSON, in the same shapes as
  the configuration above (`modules`, `external`, `limits`, capabilities)
  and as `ModuleInfo`. A binding needs a JSON library and nothing else.
- Every fallible function returns a status and leaves text in its last
  argument: the result, or the error message.
- The host's side (host commands, the load gate, events, logging) is
  callbacks with a `user_data` pointer. A callback answers by calling
  `wasm_backend_reply_ok` or `wasm_backend_reply_err`, so no memory changes
  hands.
- Everything the Rust API offers is there: loading from files or memory,
  events both ways, state, `inactive`/`enable`/`disable`, hot reload.

[`hosts/`](hosts) has a host in C and a binding with a host in Python
(`ctypes`, no build step):

```python
from wasm_backend import Backend

backend = Backend("libwasm_backend.so", {"modules": {"greeter": {"path": "greeter.wasm"}}},
                  host_commands={"now": lambda module, args: time.time()})
backend.invoke("greeter", "greet", {"name": "Python"})   # 'Hello, Python!'
```

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

The hosts in [`hosts/`](hosts) are built and run the same way, by the tests
of `wasm-backend-ffi`: a C compiler (`cc`) and `python3` are used if they
are installed, and required with `WASM_CORE_REQUIRE_ALL_HOSTS=1`.

## Wasmtime compile times

Wasmtime compiles WASM to native code when a module loads, which is slow in an
unoptimized build (seconds for a JavaScript module). Hosts should optimize the
compiler crates in dev builds; see the `[profile.dev.package.*]` sections of
the Tauri plugin's example app.

## License

The repository is licensed under the [Boost Software License 1.0](LICENSE),
the guests and hosts included.
The two crates are licensed under the
[Apache License 2.0 with LLVM Exceptions](crates/wasm-backend-core/LICENSE)
(`Apache-2.0 WITH LLVM-exception`): `wasm-backend-core` keeps the license it
was published under in `tauri-plugin-wasm-backend`, and
[`wasm-backend-ffi`](crates/wasm-backend-ffi/LICENSE) uses the same one.
