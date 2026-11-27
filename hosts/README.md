# Hosts

Apps in other languages that run modules through the C interface
([`wasm-backend-ffi`](../crates/wasm-backend-ffi)), grouped by language:
`hosts/<language>`. The tests of `wasm-backend-ffi` build and run each one
against the library and header they were just built from, so a change to the
interface is checked against every host language.

| Host                                   | Language | Uses                                                        |
|----------------------------------------|----------|-------------------------------------------------------------|
| [`c/host.c`](c/host.c)                 | C        | `wasm_backend.h` directly                                   |
| [`python/host.py`](python/host.py)     | Python   | [`python/wasm_backend.py`](python/wasm_backend.py), a `ctypes` binding |

Both load [`guests/rust/component-guest`](../guests/rust/component-guest),
give it a host command, call it, and show a module's own error and a refused
capability. The Python host also uses a load gate and lists what isn't
running.

```sh
cargo build -p wasm-backend-ffi                # target/debug/libwasm_backend.*
python3 hosts/python/host.py target/debug/libwasm_backend.so path/to/component_guest.wasm
```

## Writing a binding

[`python/wasm_backend.py`](python/wasm_backend.py) is a complete binding in
under 200 lines and a fair template for another language:

1. Load the library and declare the functions of
   [`wasm_backend.h`](../crates/wasm-backend-ffi/include/wasm_backend.h).
2. Wrap the calling convention once: pass `&out`, read the text, free it with
   `wasm_backend_string_free`, turn a status other than
   `WASM_BACKEND_STATUS_OK` into your language's error (with
   `WASM_BACKEND_STATUS_COMMAND`, the text is the module's error value as
   JSON).
3. Wrap callbacks once: decode the JSON input, call the user's function,
   answer with `wasm_backend_reply_ok` or `wasm_backend_reply_err`. Catch
   exceptions before they reach C, and keep the callbacks alive as long as
   the backend.

Callbacks arrive on whichever thread the work happens on (a module's event
handler runs on a thread of its own), so they must be safe to call from any
thread.

## Adding a language

1. Put the host in `hosts/<language>`. It takes the module to load (and, if
   it loads the library at runtime, the library) as arguments and prints
   what it does.
2. Add a test next to `c_host` and `python_host` in
   [`crates/wasm-backend-ffi/tests/hosts.rs`](../crates/wasm-backend-ffi/tests/hosts.rs)
   that builds and runs it and checks its output. Look the toolchain up with
   `tool`, which skips the test if it is missing unless
   `WASM_CORE_REQUIRE_ALL_HOSTS` is set.
3. Install the toolchain in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml).
