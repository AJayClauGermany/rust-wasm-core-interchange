/*
 * C interface to wasm-backend-core: host hot-reloadable, capability-restricted
 * WebAssembly backend modules from any language that can call C.
 *
 * Generated from crates/wasm-backend-ffi/src/lib.rs. Do not edit.
 *
 * Strings are NUL-terminated UTF-8 and structured values are JSON. Fallible
 * functions return a WasmBackendStatus and leave text in their last argument,
 * `out`: the result on WASM_BACKEND_STATUS_OK (NULL if there is none), the
 * error message otherwise (with WASM_BACKEND_STATUS_COMMAND, the error value
 * the module returned, as JSON). Free it with wasm_backend_string_free; pass
 * NULL for `out` if the text isn't wanted.
 *
 * A backend may be used from any thread. Callbacks are called on whichever
 * thread the work happens on, get strings that are only valid until they
 * return, and must return normally (no exceptions or longjmp across them).
 */

#ifndef WASM_BACKEND_H
#define WASM_BACKEND_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

// What a function call came to. Anything but `OK` leaves the error message
// in the function's `out` argument.
enum WasmBackendStatus
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
  WASM_BACKEND_STATUS_OK = 0,
  // An argument was `NULL`, not UTF-8 or not the JSON that was expected.
  WASM_BACKEND_STATUS_INVALID_ARGUMENT = 1,
  // A file could not be read or written.
  WASM_BACKEND_STATUS_IO = 2,
  // No module of that name is loaded.
  WASM_BACKEND_STATUS_MODULE_NOT_FOUND = 3,
  // The module has no such command.
  WASM_BACKEND_STATUS_COMMAND_NOT_FOUND = 4,
  // The module could not be loaded.
  WASM_BACKEND_STATUS_LOAD = 5,
  // The arguments don't fit the command's parameters.
  WASM_BACKEND_STATUS_INVALID_ARGS = 6,
  // The command ran and returned an error. `out` is that error value as
  // JSON, not a message.
  WASM_BACKEND_STATUS_COMMAND = 7,
  // The module trapped (a panic, a timeout, out of memory) or returned
  // malformed output. Its memory was reset.
  WASM_BACKEND_STATUS_RUNTIME = 8,
  // The module trapped more often than its `maxRestarts` allows and must
  // be reloaded.
  WASM_BACKEND_STATUS_STOPPED = 9,
  // The module is switched off until it is enabled.
  WASM_BACKEND_STATUS_DISABLED = 10,
  // The module's state could not be read or written.
  WASM_BACKEND_STATUS_STATE = 11,
  // Refused: by the load gate, the plugin lists or a pinned hash.
  WASM_BACKEND_STATUS_NOT_ALLOWED = 12,
  // A host command called back into the module that invoked it.
  WASM_BACKEND_STATUS_REENTRANT = 13,
  // The library was built without the engine the module needs.
  WASM_BACKEND_STATUS_ENGINE_DISABLED = 14,
  // A directory the configuration refers to is not known.
  WASM_BACKEND_STATUS_PLATFORM = 15,
  // A bug in this library. The backend is still usable.
  WASM_BACKEND_STATUS_PANIC = 16,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum WasmBackendStatus WasmBackendStatus;
#else
typedef int32_t WasmBackendStatus;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// A running backend: the module registry. Create one with
// `wasm_backend_builder_build`, free it with `wasm_backend_free`.
typedef struct WasmBackend WasmBackend;

// Configures a backend. Create one with `wasm_backend_builder_new`, add
// callbacks, then turn it into a backend with `wasm_backend_builder_build`.
typedef struct WasmBackendBuilder WasmBackendBuilder;

// Where a `WasmBackendCallFn` leaves its answer, with
// `wasm_backend_reply_ok` or `wasm_backend_reply_err`. Only valid during
// the call it was passed to.
typedef struct WasmBackendReply WasmBackendReply;

// Receives every event the backend emits: the events modules emit (after
// their capabilities were checked) and lifecycle events such as
// `wasm-backend://module-loaded`. `payload_json` is the event's payload.
typedef void (*WasmBackendEventFn)(void *user_data, const char *event, const char *payload_json);

// A question the backend asks the host, answered through `reply`.
//
// As a host command: `module` is the module calling it and `input_json`
// its arguments. Answer with `wasm_backend_reply_ok` (the result as JSON)
// or `wasm_backend_reply_err`.
//
// As a load gate: `module` is the name of the module about to be loaded
// and `input_json` describes it: `{"name", "path", "external", "sha256",
// "capabilities", "reload"}`. `wasm_backend_reply_ok` lets it load,
// `wasm_backend_reply_err` refuses it with a reason.
//
// Returning without replying is an error for a host command, and refuses
// the load for a gate.
typedef void (*WasmBackendCallFn)(void *user_data,
                                  const char *module,
                                  const char *input_json,
                                  struct WasmBackendReply *reply);

// Receives log messages: those of the backend and those modules write with
// the host's `log` function (target `wasm_backend`). `level` is 1 for
// errors, 2 warnings, 3 info, 4 debug and 5 trace.
typedef void (*WasmBackendLogFn)(void *user_data,
                                 int32_t level,
                                 const char *target,
                                 const char *message);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// Start configuring a backend.
//
// `config_json` is a JSON object, or `NULL` for an empty one. Its keys are
// those of the core's configuration (`modules`, `external`, `limits`,
// `hotReload`, `typescriptOutDir`, `stateDir`), plus:
//
// - `dirs`: the directories `$APPDATA`-style paths refer to, as
//   `{"appData", "appLocalData", "appCache", "appConfig", "resource",
//   "temp"}`. `resource` (which relative module paths are resolved
//   against) defaults to the current directory and `temp` to the system's;
//   the others default to `appData`, which has no default.
// - `dev`: `true` loads modules from their `devPath`. Defaults to `false`.
//
// Hot reload is off unless `hotReload` is `true`, and module state is only
// kept between runs if `stateDir` is set.
//
// On success `*builder` is the new builder, to be passed to
// `wasm_backend_builder_build` or `wasm_backend_builder_free`.
WasmBackendStatus wasm_backend_builder_new(const char *config_json,
                                           struct WasmBackendBuilder **builder,
                                           char **out);

// Free a builder that is not going to be built. `NULL` is ignored.
void wasm_backend_builder_free(struct WasmBackendBuilder *builder);

// Call `callback` for every event the backend emits (see
// `WasmBackendEventFn`). Events emitted while
// `wasm_backend_builder_build` loads the configured modules arrive before
// it returns.
void wasm_backend_builder_on_event(struct WasmBackendBuilder *builder,
                                   WasmBackendEventFn callback,
                                   void *user_data);

// Register a host command: a function modules can call, if their
// `hostCommands` capability allows it, by name through the host's `invoke`
// or as a typed function a WIT module imports. Names match across
// `snake_case`, `kebab-case` and `camelCase`.
WasmBackendStatus wasm_backend_builder_host_command(struct WasmBackendBuilder *builder,
                                                    const char *name,
                                                    WasmBackendCallFn callback,
                                                    void *user_data,
                                                    char **out);

// Ask `callback` before any module is loaded or reloaded, and before any
// of it is compiled (see `WasmBackendCallFn`). Use it to decide which
// third-party plugins run, e.g. by comparing `sha256` with what the user
// approved. The same module may be asked about more than once.
void wasm_backend_builder_load_gate(struct WasmBackendBuilder *builder,
                                    WasmBackendCallFn callback,
                                    void *user_data);

// Create the backend and load the configured modules. This consumes
// `builder`, whether it succeeds or not.
//
// A module that fails to load doesn't fail the build: it is reported as a
// `wasm-backend://module-error` event and listed by
// `wasm_backend_inactive`. On success `*backend` is the new backend.
WasmBackendStatus wasm_backend_builder_build(struct WasmBackendBuilder *builder,
                                             struct WasmBackend **backend,
                                             char **out);

// Answer a `WasmBackendCallFn` with a result: `json` is the host command's
// return value as JSON (`NULL` for none). For a load gate it lets the
// module load, and `json` is ignored.
void wasm_backend_reply_ok(struct WasmBackendReply *reply, const char *json);

// Answer a `WasmBackendCallFn` with an error: the host command failed with
// `message`, or the load gate refuses the module for that reason.
void wasm_backend_reply_err(struct WasmBackendReply *reply, const char *message);

// Free a backend. Modules stop running; their stored state is kept.
// `NULL` is ignored. No other call may be using the backend.
void wasm_backend_free(struct WasmBackend *backend);

// Free a string this library returned through an `out` argument. `NULL`
// is ignored.
void wasm_backend_string_free(char *string);

// Invoke a module's command. `args_json` is an object keyed by parameter
// name for WIT modules and the JSON input for Extism modules (`NULL` for
// none). `*out` is the result as JSON.
//
// The call runs on the calling thread and returns when the command does.
WasmBackendStatus wasm_backend_invoke(const struct WasmBackend *backend,
                                      const char *module,
                                      const char *command,
                                      const char *args_json,
                                      char **out);

// Tell the modules that an app event happened: every module whose
// `events.listen` capability names `event` receives it, on another thread.
// The event callback is not called; the host knows already.
WasmBackendStatus wasm_backend_emit(const struct WasmBackend *backend,
                                    const char *event,
                                    const char *payload_json,
                                    char **out);

// Deliver an event to one module's event handler, whatever it listens to,
// and wait for the handler to return.
WasmBackendStatus wasm_backend_send_event(const struct WasmBackend *backend,
                                          const char *module,
                                          const char *event,
                                          const char *payload_json,
                                          char **out);

// Every loaded module, as a JSON array of module descriptions: name,
// engine, path, `sha256`, commands, capabilities, what it imports, how
// often it trapped, ...
WasmBackendStatus wasm_backend_modules(const struct WasmBackend *backend, char **out);

// Every module that is known but not running, as a JSON array of `{"name",
// "path", "external", "sha256", "reason", "message", "capabilities"}`.
// `reason` is `"disabled"`, `"denied"` (by the load gate, the plugin lists
// or a pinned hash) or `"failed"`.
WasmBackendStatus wasm_backend_inactive(const struct WasmBackend *backend, char **out);

// One loaded module's description, as JSON.
WasmBackendStatus wasm_backend_module(const struct WasmBackend *backend,
                                      const char *name,
                                      char **out);

// Load (or replace) a module from a file. `config_json` is a module
// configuration (`{"path", "capabilities", "engine", "watch", "enabled",
// "sha256", ...}`). `*out` is the module's description.
WasmBackendStatus wasm_backend_load(const struct WasmBackend *backend,
                                    const char *name,
                                    const char *config_json,
                                    char **out);

// Load (or replace) a module from memory. The bytes are copied.
// `config_json` is a module configuration without `path` (`NULL` for the
// default: no capabilities). `*out` is the module's description.
WasmBackendStatus wasm_backend_load_bytes(const struct WasmBackend *backend,
                                          const char *name,
                                          const uint8_t *bytes,
                                          uintptr_t bytes_len,
                                          const char *config_json,
                                          char **out);

// Unload a module and forget it. Its stored state is kept.
WasmBackendStatus wasm_backend_unload(const struct WasmBackend *backend,
                                      const char *name,
                                      char **out);

// Reload a module from its file, which resets its memory and starts a
// stopped module again. `*out` is the module's description.
WasmBackendStatus wasm_backend_reload(const struct WasmBackend *backend,
                                      const char *name,
                                      char **out);

// Switch a module off: it is unloaded and stays that way until
// `wasm_backend_enable`, listed by `wasm_backend_inactive`.
WasmBackendStatus wasm_backend_disable(const struct WasmBackend *backend,
                                       const char *name,
                                       char **out);

// Load a module `wasm_backend_inactive` lists: one that was switched off,
// or one that was refused or failed and should be tried again. `*out` is
// the module's description.
WasmBackendStatus wasm_backend_enable(const struct WasmBackend *backend,
                                      const char *name,
                                      char **out);

// Everything a module stored in its state store, as a JSON object of
// strings, whether or not the module is loaded.
WasmBackendStatus wasm_backend_state(const struct WasmBackend *backend,
                                     const char *module,
                                     char **out);

// Remove everything a module stored.
WasmBackendStatus wasm_backend_clear_state(const struct WasmBackend *backend,
                                           const char *module,
                                           char **out);

// TypeScript bindings for a module's commands. `*out` is the source text,
// not JSON.
WasmBackendStatus wasm_backend_typescript(const struct WasmBackend *backend,
                                          const char *module,
                                          char **out);

// Scan the external plugins directory now: load new plugins, try the ones
// that were refused or failed again, and unload those whose file is gone.
WasmBackendStatus wasm_backend_sync_external(const struct WasmBackend *backend, char **out);

// Send log messages up to `max_level` (1 errors only ... 5 everything) to
// `callback`, for the whole process. Without this, what the backend and
// its modules log goes nowhere. It can be set once.
WasmBackendStatus wasm_backend_set_logger(WasmBackendLogFn callback,
                                          void *user_data,
                                          int32_t max_level,
                                          char **out);

// The version of this library, e.g. `"0.1.0"`. Not to be freed.
const char *wasm_backend_version(void);

// The WIT package modules are built against (`tauri:backend`), for tools
// that hand it to guest builds. Not to be freed.
const char *wasm_backend_wit(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* WASM_BACKEND_H */
