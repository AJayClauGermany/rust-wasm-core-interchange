//! C interface to [`wasm_backend_core`]: host hot-reloadable,
//! capability-restricted WebAssembly backend modules from any language that
//! can call C (C and C++, Python, Go, Swift, C#, Java, ...).
//!
//! The core stays a Rust library; this crate wraps it for everyone else. It
//! builds `libwasm_backend` (shared and static) and its functions are
//! declared in `include/wasm_backend.h`.
//!
//! # Conventions
//!
//! - **Strings** are NUL-terminated UTF-8. Structured values (configuration,
//!   arguments, results, module descriptions) are JSON, in the shapes the
//!   core's types serialize to, so a binding needs a JSON library and
//!   nothing else.
//! - **Results.** Fallible functions return a [`WasmBackendStatus`] and take
//!   an `out` pointer as their last argument. On
//!   [`WasmBackendStatus::Ok`] `*out` is the result (or `NULL` if the
//!   function has none); otherwise it is the error message. One exception:
//!   with [`WasmBackendStatus::Command`] it is the error *value* the module
//!   returned, as JSON. Free it with [`wasm_backend_string_free`]. `out`
//!   may be `NULL` if the text isn't wanted.
//! - **Threads.** A backend may be used from any thread, and from several
//!   at once. Callbacks are called on whichever thread the work happens on,
//!   which is often not the one that registered them, so they and their
//!   `user_data` must be safe to use from any thread.
//! - **Callbacks** get strings that are only valid until they return, and
//!   must return normally: no exceptions or `longjmp` across them.
//!
//! ```c
//! WasmBackendBuilder *builder;
//! char *out;
//! wasm_backend_builder_new(
//!     "{\"modules\": {\"greeter\": {\"path\": \"greeter.wasm\"}}}", &builder, &out);
//! WasmBackend *backend;
//! wasm_backend_builder_build(builder, &backend, &out);
//! if (wasm_backend_invoke(backend, "greeter", "greet", "{\"name\": \"C\"}", &out)
//!     == WASM_BACKEND_STATUS_OK)
//!   puts(out); // "Hello, C!"
//! wasm_backend_string_free(out);
//! wasm_backend_free(backend);
//! ```

// Each function documents what it expects of its pointers; the rules they
// share are in the crate documentation above.
#![allow(clippy::missing_safety_doc)]

use std::{
    ffi::{CStr, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::Arc,
};

use serde::Serialize;
use serde_json::Value;
use wasm_backend_core::{Backend, BaseDir, Error, FileStateStore, ModuleConfig, resolve_dir};

mod platform;

use platform::{EventSink, FfiPlatform, Options, c_string};

/// What a function call came to. Anything but `OK` leaves the error message
/// in the function's `out` argument.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmBackendStatus {
    Ok = 0,
    /// An argument was `NULL`, not UTF-8 or not the JSON that was expected.
    InvalidArgument = 1,
    /// A file could not be read or written.
    Io = 2,
    /// No module of that name is loaded.
    ModuleNotFound = 3,
    /// The module has no such command.
    CommandNotFound = 4,
    /// The module could not be loaded.
    Load = 5,
    /// The arguments don't fit the command's parameters.
    InvalidArgs = 6,
    /// The command ran and returned an error. `out` is that error value as
    /// JSON, not a message.
    Command = 7,
    /// The module trapped (a panic, a timeout, out of memory) or returned
    /// malformed output. Its memory was reset.
    Runtime = 8,
    /// The module trapped more often than its `maxRestarts` allows and must
    /// be reloaded.
    Stopped = 9,
    /// The module is switched off until it is enabled.
    Disabled = 10,
    /// The module's state could not be read or written.
    State = 11,
    /// Refused: by the load gate, the plugin lists or a pinned hash.
    NotAllowed = 12,
    /// A host command called back into the module that invoked it.
    Reentrant = 13,
    /// The library was built without the engine the module needs.
    EngineDisabled = 14,
    /// A directory the configuration refers to is not known.
    Platform = 15,
    /// A bug in this library. The backend is still usable.
    Panic = 16,
}

/// A running backend: the module registry. Create one with
/// `wasm_backend_builder_build`, free it with `wasm_backend_free`.
pub struct WasmBackend(Backend<FfiPlatform>);

/// Configures a backend. Create one with `wasm_backend_builder_new`, add
/// callbacks, then turn it into a backend with `wasm_backend_builder_build`.
pub struct WasmBackendBuilder {
    options: Options,
    on_event: Option<EventSink>,
    host_commands: Vec<(String, Callback)>,
    load_gate: Option<Callback>,
}

/// Where a `WasmBackendCallFn` leaves its answer, with
/// `wasm_backend_reply_ok` or `wasm_backend_reply_err`. Only valid during
/// the call it was passed to.
pub struct WasmBackendReply(Option<Result<Value, String>>);

/// Receives every event the backend emits: the events modules emit (after
/// their capabilities were checked) and lifecycle events such as
/// `wasm-backend://module-loaded`. `payload_json` is the event's payload.
pub type WasmBackendEventFn = Option<
    unsafe extern "C" fn(user_data: *mut c_void, event: *const c_char, payload_json: *const c_char),
>;

/// A question the backend asks the host, answered through `reply`.
///
/// As a host command: `module` is the module calling it and `input_json`
/// its arguments. Answer with `wasm_backend_reply_ok` (the result as JSON)
/// or `wasm_backend_reply_err`.
///
/// As a load gate: `module` is the name of the module about to be loaded
/// and `input_json` describes it: `{"name", "path", "external", "sha256",
/// "capabilities", "reload"}`. `wasm_backend_reply_ok` lets it load,
/// `wasm_backend_reply_err` refuses it with a reason.
///
/// Returning without replying is an error for a host command, and refuses
/// the load for a gate.
pub type WasmBackendCallFn = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        module: *const c_char,
        input_json: *const c_char,
        reply: *mut WasmBackendReply,
    ),
>;

/// Receives log messages: those of the backend and those modules write with
/// the host's `log` function (target `wasm_backend`). `level` is 1 for
/// errors, 2 warnings, 3 info, 4 debug and 5 trace.
pub type WasmBackendLogFn = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        level: i32,
        target: *const c_char,
        message: *const c_char,
    ),
>;

#[derive(Clone, Copy)]
struct Callback {
    f: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *mut WasmBackendReply),
    user_data: *mut c_void,
}

// The host promises that its callbacks and their `user_data` may be used
// from any thread (see the crate documentation).
unsafe impl Send for Callback {}
unsafe impl Sync for Callback {}

impl Callback {
    /// Ask the host; `None` if it returned without replying.
    fn call(&self, module: &str, input: &str) -> Option<Result<Value, String>> {
        let (module, input) = (c_string(module), c_string(input));
        let mut reply = WasmBackendReply(None);
        // SAFETY: the host registered this callback for these arguments,
        // which stay valid for the duration of the call.
        unsafe { (self.f)(self.user_data, module.as_ptr(), input.as_ptr(), &mut reply) };
        reply.0
    }
}

/// Why a call failed.
type Failure = (WasmBackendStatus, String);
/// What a call leaves in `out`.
type Outcome = Result<Option<String>, Failure>;

fn invalid(message: impl Into<String>) -> Failure {
    (WasmBackendStatus::InvalidArgument, message.into())
}

fn failure(error: Error) -> Failure {
    use WasmBackendStatus as S;
    let status = match &error {
        // The module's own error value, for the caller to take apart.
        Error::Command(value) => return (S::Command, value.to_string()),
        Error::Io(_) => S::Io,
        Error::Platform(_) => S::Platform,
        Error::ModuleNotFound(_) => S::ModuleNotFound,
        Error::CommandNotFound { .. } => S::CommandNotFound,
        Error::Load { .. } => S::Load,
        Error::InvalidArgs { .. } => S::InvalidArgs,
        Error::Runtime(_) => S::Runtime,
        Error::Stopped { .. } => S::Stopped,
        Error::Disabled(_) => S::Disabled,
        Error::State(_) => S::State,
        Error::NotAllowed(_) => S::NotAllowed,
        Error::Reentrant(_) => S::Reentrant,
        Error::EngineDisabled(_) => S::EngineDisabled,
    };
    (status, error.to_string())
}

fn json(value: &impl Serialize) -> Outcome {
    serde_json::to_string(value)
        .map(Some)
        .map_err(|e| (WasmBackendStatus::Runtime, e.to_string()))
}

/// Run `f`, leave its text in `out` and return its status. A panic becomes
/// a status too: unwinding into the caller would be undefined behavior.
unsafe fn finish(out: *mut *mut c_char, f: impl FnOnce() -> Outcome) -> WasmBackendStatus {
    let outcome = catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_string());
        Err((WasmBackendStatus::Panic, message))
    });
    let (status, text) = match outcome {
        Ok(text) => (WasmBackendStatus::Ok, text),
        Err((status, message)) => (status, Some(message)),
    };
    if !out.is_null() {
        let text = text.map_or(ptr::null_mut(), |t| c_string(&t).into_raw());
        // SAFETY: the caller passed a pointer to write the text to.
        unsafe { *out = text };
    }
    status
}

/// A required string argument.
unsafe fn text<'a>(pointer: *const c_char, name: &str) -> Result<&'a str, Failure> {
    if pointer.is_null() {
        return Err(invalid(format!("`{name}` is NULL")));
    }
    // SAFETY: the caller passed a NUL-terminated string that outlives the call.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map_err(|_| invalid(format!("`{name}` is not UTF-8")))
}

/// A JSON argument; `NULL` and the empty string are `null`.
unsafe fn json_arg(pointer: *const c_char, name: &str) -> Result<Value, Failure> {
    if pointer.is_null() {
        return Ok(Value::Null);
    }
    let text = unsafe { text(pointer, name) }?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(text).map_err(|e| invalid(format!("`{name}` is not JSON: {e}")))
}

/// A handle argument.
unsafe fn handle<'a, T>(pointer: *const T, name: &str) -> Result<&'a T, Failure> {
    // SAFETY: the caller passed a handle this library returned and has not
    // freed.
    unsafe { pointer.as_ref() }.ok_or_else(|| invalid(format!("`{name}` is NULL")))
}

/// A module configuration; `NULL` is the default one.
unsafe fn module_config(pointer: *const c_char) -> Result<ModuleConfig, Failure> {
    match unsafe { json_arg(pointer, "config_json") }? {
        Value::Null => Ok(ModuleConfig::default()),
        config => serde_json::from_value(config)
            .map_err(|e| invalid(format!("invalid module configuration: {e}"))),
    }
}

/// Start configuring a backend.
///
/// `config_json` is a JSON object, or `NULL` for an empty one. Its keys are
/// those of the core's configuration (`modules`, `external`, `limits`,
/// `hotReload`, `typescriptOutDir`, `stateDir`), plus:
///
/// - `dirs`: the directories `$APPDATA`-style paths refer to, as
///   `{"appData", "appLocalData", "appCache", "appConfig", "resource",
///   "temp"}`. `resource` (which relative module paths are resolved
///   against) defaults to the current directory and `temp` to the system's;
///   the others default to `appData`, which has no default.
/// - `dev`: `true` loads modules from their `devPath`. Defaults to `false`.
///
/// Hot reload is off unless `hotReload` is `true`, and module state is only
/// kept between runs if `stateDir` is set.
///
/// On success `*builder` is the new builder, to be passed to
/// `wasm_backend_builder_build` or `wasm_backend_builder_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_builder_new(
    config_json: *const c_char,
    builder: *mut *mut WasmBackendBuilder,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            if builder.is_null() {
                return Err(invalid("`builder` is NULL"));
            }
            *builder = ptr::null_mut();
            let options = Options::parse(json_arg(config_json, "config_json")?).map_err(invalid)?;
            *builder = Box::into_raw(Box::new(WasmBackendBuilder {
                options,
                on_event: None,
                host_commands: Vec::new(),
                load_gate: None,
            }));
            Ok(None)
        })
    }
}

/// Free a builder that is not going to be built. `NULL` is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_builder_free(builder: *mut WasmBackendBuilder) {
    if !builder.is_null() {
        // SAFETY: the caller owns the builder and gives it up here.
        drop(unsafe { Box::from_raw(builder) });
    }
}

/// Call `callback` for every event the backend emits (see
/// `WasmBackendEventFn`). Events emitted while
/// `wasm_backend_builder_build` loads the configured modules arrive before
/// it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_builder_on_event(
    builder: *mut WasmBackendBuilder,
    callback: WasmBackendEventFn,
    user_data: *mut c_void,
) {
    // SAFETY: the caller passed a builder it owns.
    if let Some(builder) = unsafe { builder.as_mut() } {
        builder.on_event = callback.map(|f| EventSink { f, user_data });
    }
}

/// Register a host command: a function modules can call, if their
/// `hostCommands` capability allows it, by name through the host's `invoke`
/// or as a typed function a WIT module imports. Names match across
/// `snake_case`, `kebab-case` and `camelCase`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_builder_host_command(
    builder: *mut WasmBackendBuilder,
    name: *const c_char,
    callback: WasmBackendCallFn,
    user_data: *mut c_void,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let builder = builder
                .as_mut()
                .ok_or_else(|| invalid("`builder` is NULL"))?;
            let name = text(name, "name")?.to_string();
            let f = callback.ok_or_else(|| invalid("`callback` is NULL"))?;
            builder
                .host_commands
                .push((name, Callback { f, user_data }));
            Ok(None)
        })
    }
}

/// Ask `callback` before any module is loaded or reloaded, and before any
/// of it is compiled (see `WasmBackendCallFn`). Use it to decide which
/// third-party plugins run, e.g. by comparing `sha256` with what the user
/// approved. The same module may be asked about more than once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_builder_load_gate(
    builder: *mut WasmBackendBuilder,
    callback: WasmBackendCallFn,
    user_data: *mut c_void,
) {
    // SAFETY: the caller passed a builder it owns.
    if let Some(builder) = unsafe { builder.as_mut() } {
        builder.load_gate = callback.map(|f| Callback { f, user_data });
    }
}

/// Create the backend and load the configured modules. This consumes
/// `builder`, whether it succeeds or not.
///
/// A module that fails to load doesn't fail the build: it is reported as a
/// `wasm-backend://module-error` event and listed by
/// `wasm_backend_inactive`. On success `*backend` is the new backend.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_builder_build(
    builder: *mut WasmBackendBuilder,
    backend: *mut *mut WasmBackend,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            if !backend.is_null() {
                *backend = ptr::null_mut();
            }
            if builder.is_null() {
                return Err(invalid("`builder` is NULL"));
            }
            let builder = Box::from_raw(builder);
            if backend.is_null() {
                return Err(invalid("`backend` is NULL"));
            }
            let built = build(*builder).map_err(failure)?;
            *backend = Box::into_raw(Box::new(WasmBackend(built)));
            Ok(None)
        })
    }
}

fn build(builder: WasmBackendBuilder) -> Result<Backend<FfiPlatform>, Error> {
    let WasmBackendBuilder {
        options: Options { config, dirs, dev },
        on_event,
        host_commands,
        load_gate,
    } = builder;
    let platform = FfiPlatform::new(dirs, on_event);
    let state_dir = config
        .state_dir
        .as_deref()
        .map(|dir| resolve_dir(&platform, dir, BaseDir::AppData))
        .transpose()?;
    let typescript_dir = config
        .typescript_out_dir
        .map(|dir| std::env::current_dir().map(|current| current.join(dir)))
        .transpose()?;
    let mut core = Backend::builder(platform)
        .dev(dev)
        .hot_reload(config.hot_reload.unwrap_or(false))
        .external(config.external)
        .limits(config.limits)
        .typescript_out_dir(typescript_dir);
    if let Some(dir) = state_dir {
        core = core.state_store(FileStateStore::new(dir));
    }
    for (name, callback) in host_commands {
        let command = name.clone();
        core = core.host_command_fn(
            name,
            Arc::new(move |call, args| {
                callback
                    .call(call.module, &args.to_string())
                    .unwrap_or_else(|| {
                        Err(format!(
                            "host command `{command}` returned without replying"
                        ))
                    })
            }),
        );
    }
    if let Some(gate) = load_gate {
        core = core.load_gate(move |_, request| {
            let described = serde_json::to_string(request).map_err(|e| e.to_string())?;
            match gate.call(request.name, &described) {
                Some(answer) => answer.map(drop),
                None => Err("the load gate returned without replying".to_string()),
            }
        });
    }
    let backend = core.build()?;
    backend.load_all(config.modules);
    Ok(backend)
}

/// Answer a `WasmBackendCallFn` with a result: `json` is the host command's
/// return value as JSON (`NULL` for none). For a load gate it lets the
/// module load, and `json` is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_reply_ok(reply: *mut WasmBackendReply, json: *const c_char) {
    // SAFETY: the caller passed the reply its callback was given.
    let Some(reply) = (unsafe { reply.as_mut() }) else {
        return;
    };
    reply.0 = Some(unsafe { json_arg(json, "json") }.map_err(|(_, message)| message));
}

/// Answer a `WasmBackendCallFn` with an error: the host command failed with
/// `message`, or the load gate refuses the module for that reason.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_reply_err(
    reply: *mut WasmBackendReply,
    message: *const c_char,
) {
    // SAFETY: the caller passed the reply its callback was given.
    let Some(reply) = (unsafe { reply.as_mut() }) else {
        return;
    };
    let message = unsafe { text(message, "message") }.unwrap_or("unknown error");
    reply.0 = Some(Err(message.to_string()));
}

/// Free a backend. Modules stop running; their stored state is kept.
/// `NULL` is ignored. No other call may be using the backend.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_free(backend: *mut WasmBackend) {
    if !backend.is_null() {
        // SAFETY: the caller owns the backend and gives it up here.
        drop(unsafe { Box::from_raw(backend) });
    }
}

/// Free a string this library returned through an `out` argument. `NULL`
/// is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_string_free(string: *mut c_char) {
    if !string.is_null() {
        // SAFETY: the string came from `CString::into_raw` in `finish`.
        drop(unsafe { std::ffi::CString::from_raw(string) });
    }
}

/// Invoke a module's command. `args_json` is an object keyed by parameter
/// name for WIT modules and the JSON input for Extism modules (`NULL` for
/// none). `*out` is the result as JSON.
///
/// The call runs on the calling thread and returns when the command does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_invoke(
    backend: *const WasmBackend,
    module: *const c_char,
    command: *const c_char,
    args_json: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            let (module, command) = (text(module, "module")?, text(command, "command")?);
            let args = json_arg(args_json, "args_json")?;
            json(&backend.0.invoke(module, command, args).map_err(failure)?)
        })
    }
}

/// Tell the modules that an app event happened: every module whose
/// `events.listen` capability names `event` receives it, on another thread.
/// The event callback is not called; the host knows already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_emit(
    backend: *const WasmBackend,
    event: *const c_char,
    payload_json: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            let event = text(event, "event")?;
            let payload = json_arg(payload_json, "payload_json")?;
            backend.0.platform().bus.emit(event, &payload);
            Ok(None)
        })
    }
}

/// Deliver an event to one module's event handler, whatever it listens to,
/// and wait for the handler to return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_send_event(
    backend: *const WasmBackend,
    module: *const c_char,
    event: *const c_char,
    payload_json: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            let (module, event) = (text(module, "module")?, text(event, "event")?);
            let payload = json_arg(payload_json, "payload_json")?;
            backend
                .0
                .send_event(module, event, &payload)
                .map_err(failure)?;
            Ok(None)
        })
    }
}

/// Every loaded module, as a JSON array of module descriptions: name,
/// engine, path, `sha256`, commands, capabilities, what it imports, how
/// often it trapped, ...
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_modules(
    backend: *const WasmBackend,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe { finish(out, || json(&handle(backend, "backend")?.0.modules())) }
}

/// Every module that is known but not running, as a JSON array of `{"name",
/// "path", "external", "sha256", "reason", "message", "capabilities"}`.
/// `reason` is `"disabled"`, `"denied"` (by the load gate, the plugin lists
/// or a pinned hash) or `"failed"`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_inactive(
    backend: *const WasmBackend,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe { finish(out, || json(&handle(backend, "backend")?.0.inactive())) }
}

/// One loaded module's description, as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_module(
    backend: *const WasmBackend,
    name: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            json(&backend.0.module(text(name, "name")?).map_err(failure)?)
        })
    }
}

/// Load (or replace) a module from a file. `config_json` is a module
/// configuration (`{"path", "capabilities", "engine", "watch", "enabled",
/// "sha256", ...}`). `*out` is the module's description.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_load(
    backend: *const WasmBackend,
    name: *const c_char,
    config_json: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            let config = module_config(config_json)?;
            json(
                &backend
                    .0
                    .load(text(name, "name")?, config)
                    .map_err(failure)?,
            )
        })
    }
}

/// Load (or replace) a module from memory. The bytes are copied.
/// `config_json` is a module configuration without `path` (`NULL` for the
/// default: no capabilities). `*out` is the module's description.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_load_bytes(
    backend: *const WasmBackend,
    name: *const c_char,
    bytes: *const u8,
    bytes_len: usize,
    config_json: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            if bytes.is_null() {
                return Err(invalid("`bytes` is NULL"));
            }
            let mut config = module_config(config_json)?;
            // SAFETY: the caller passed `bytes_len` readable bytes.
            config.bytes = Some(std::slice::from_raw_parts(bytes, bytes_len).into());
            json(
                &backend
                    .0
                    .load(text(name, "name")?, config)
                    .map_err(failure)?,
            )
        })
    }
}

/// Unload a module and forget it. Its stored state is kept.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_unload(
    backend: *const WasmBackend,
    name: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            backend.0.unload(text(name, "name")?).map_err(failure)?;
            Ok(None)
        })
    }
}

/// Reload a module from its file, which resets its memory and starts a
/// stopped module again. `*out` is the module's description.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_reload(
    backend: *const WasmBackend,
    name: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            json(&backend.0.reload(text(name, "name")?).map_err(failure)?)
        })
    }
}

/// Switch a module off: it is unloaded and stays that way until
/// `wasm_backend_enable`, listed by `wasm_backend_inactive`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_disable(
    backend: *const WasmBackend,
    name: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            backend.0.disable(text(name, "name")?).map_err(failure)?;
            Ok(None)
        })
    }
}

/// Load a module `wasm_backend_inactive` lists: one that was switched off,
/// or one that was refused or failed and should be tried again. `*out` is
/// the module's description.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_enable(
    backend: *const WasmBackend,
    name: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            json(&backend.0.enable(text(name, "name")?).map_err(failure)?)
        })
    }
}

/// Everything a module stored in its state store, as a JSON object of
/// strings, whether or not the module is loaded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_state(
    backend: *const WasmBackend,
    module: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            json(&backend.0.state(text(module, "module")?).map_err(failure)?)
        })
    }
}

/// Remove everything a module stored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_clear_state(
    backend: *const WasmBackend,
    module: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            backend
                .0
                .clear_state(text(module, "module")?)
                .map_err(failure)?;
            Ok(None)
        })
    }
}

/// TypeScript bindings for a module's commands. `*out` is the source text,
/// not JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_typescript(
    backend: *const WasmBackend,
    module: *const c_char,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let backend = handle(backend, "backend")?;
            let source = backend.0.typescript(text(module, "module")?);
            Ok(Some(source.map_err(failure)?))
        })
    }
}

/// Scan the external plugins directory now: load new plugins, try the ones
/// that were refused or failed again, and unload those whose file is gone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_sync_external(
    backend: *const WasmBackend,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            handle(backend, "backend")?.0.sync_external();
            Ok(None)
        })
    }
}

struct Logger {
    f: unsafe extern "C" fn(*mut c_void, i32, *const c_char, *const c_char),
    user_data: *mut c_void,
}

// The host promises that its callbacks and their `user_data` may be used
// from any thread (see the crate documentation).
unsafe impl Send for Logger {}
unsafe impl Sync for Logger {}

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let target = c_string(record.target());
        let message = c_string(&record.args().to_string());
        // SAFETY: the host registered this callback for these arguments,
        // which stay valid for the duration of the call.
        unsafe {
            (self.f)(
                self.user_data,
                record.level() as i32,
                target.as_ptr(),
                message.as_ptr(),
            )
        };
    }

    fn flush(&self) {}
}

/// Send log messages up to `max_level` (1 errors only ... 5 everything) to
/// `callback`, for the whole process. Without this, what the backend and
/// its modules log goes nowhere. It can be set once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_backend_set_logger(
    callback: WasmBackendLogFn,
    user_data: *mut c_void,
    max_level: i32,
    out: *mut *mut c_char,
) -> WasmBackendStatus {
    unsafe {
        finish(out, || {
            let f = callback.ok_or_else(|| invalid("`callback` is NULL"))?;
            let level = match max_level {
                i32::MIN..=0 => log::LevelFilter::Off,
                1 => log::LevelFilter::Error,
                2 => log::LevelFilter::Warn,
                3 => log::LevelFilter::Info,
                4 => log::LevelFilter::Debug,
                _ => log::LevelFilter::Trace,
            };
            log::set_boxed_logger(Box::new(Logger { f, user_data }))
                .map_err(|_| invalid("a logger is already set for this process"))?;
            log::set_max_level(level);
            Ok(None)
        })
    }
}

/// The version of this library, e.g. `"0.1.0"`. Not to be freed.
#[unsafe(no_mangle)]
pub extern "C" fn wasm_backend_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The WIT package modules are built against (`tauri:backend`), for tools
/// that hand it to guest builds. Not to be freed.
#[unsafe(no_mangle)]
pub extern "C" fn wasm_backend_wit() -> *const c_char {
    static WIT: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();
    WIT.get_or_init(|| c_string(wasm_backend_core::WIT))
        .as_ptr()
}
