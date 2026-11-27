//! Drives the C interface the way a host would, with real guest modules.

#[path = "../../wasm-backend-core/tests/common/mod.rs"]
mod common;

use std::{
    ffi::{CStr, CString, c_char, c_void},
    ptr,
    sync::Mutex,
};

use serde_json::{Value, json};
use wasm_backend::*;

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Call a function that leaves text in `out`, and take (and free) the text.
fn call(f: impl FnOnce(*mut *mut c_char) -> WasmBackendStatus) -> (WasmBackendStatus, String) {
    let mut out: *mut c_char = ptr::null_mut();
    let status = f(&mut out);
    let text = if out.is_null() {
        String::new()
    } else {
        let text = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { wasm_backend_string_free(out) };
        text
    };
    (status, text)
}

/// The JSON result of a call that is expected to succeed.
fn ok(f: impl FnOnce(*mut *mut c_char) -> WasmBackendStatus) -> Value {
    let (status, text) = call(f);
    assert_eq!(status, WasmBackendStatus::Ok, "{text}");
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    }
}

/// What the host's callbacks saw, reached through `user_data`.
#[derive(Default)]
struct Host {
    events: Mutex<Vec<(String, Value)>>,
    asked: Mutex<Vec<Value>>,
}

fn str_of<'a>(pointer: *const c_char) -> &'a str {
    unsafe { CStr::from_ptr(pointer) }.to_str().unwrap()
}

unsafe extern "C" fn on_event(
    user_data: *mut c_void,
    event: *const c_char,
    payload: *const c_char,
) {
    let host = unsafe { &*(user_data as *const Host) };
    let payload = serde_json::from_str(str_of(payload)).unwrap();
    host.events
        .lock()
        .unwrap()
        .push((str_of(event).to_string(), payload));
}

/// The `echo` host command: answers with who called and with what.
unsafe extern "C" fn echo(
    _: *mut c_void,
    module: *const c_char,
    args: *const c_char,
    reply: *mut WasmBackendReply,
) {
    let args: Value = serde_json::from_str(str_of(args)).unwrap();
    if args == json!("fail") {
        unsafe { wasm_backend_reply_err(reply, c("echo was asked to fail").as_ptr()) };
    } else if args != json!("silent") {
        let answer = json!({ "module": str_of(module), "args": args }).to_string();
        unsafe { wasm_backend_reply_ok(reply, c(&answer).as_ptr()) };
    }
}

/// A load gate that refuses modules called `stranger`.
unsafe extern "C" fn gate(
    user_data: *mut c_void,
    module: *const c_char,
    request: *const c_char,
    reply: *mut WasmBackendReply,
) {
    let host = unsafe { &*(user_data as *const Host) };
    let request: Value = serde_json::from_str(str_of(request)).unwrap();
    assert_eq!(request["name"], str_of(module));
    host.asked.lock().unwrap().push(request);
    if str_of(module) == "stranger" {
        unsafe { wasm_backend_reply_err(reply, c("never seen before").as_ptr()) };
    } else {
        unsafe { wasm_backend_reply_ok(reply, ptr::null()) };
    }
}

fn build(config: Value, host: &Host) -> *mut WasmBackend {
    let user_data = host as *const Host as *mut c_void;
    let mut builder = ptr::null_mut();
    let config = c(&config.to_string());
    ok(|out| unsafe { wasm_backend_builder_new(config.as_ptr(), &mut builder, out) });
    unsafe {
        wasm_backend_builder_on_event(builder, Some(on_event), user_data);
        wasm_backend_builder_load_gate(builder, Some(gate), user_data);
    }
    ok(|out| unsafe {
        wasm_backend_builder_host_command(
            builder,
            c("echo").as_ptr(),
            Some(echo),
            ptr::null_mut(),
            out,
        )
    });
    let mut backend = ptr::null_mut();
    ok(|out| unsafe { wasm_backend_builder_build(builder, &mut backend, out) });
    assert!(!backend.is_null());
    backend
}

fn invoke(
    backend: *mut WasmBackend,
    module: &str,
    command: &str,
    args: Value,
) -> (WasmBackendStatus, String) {
    let (module, command, args) = (c(module), c(command), c(&args.to_string()));
    call(|out| unsafe {
        wasm_backend_invoke(
            backend,
            module.as_ptr(),
            command.as_ptr(),
            args.as_ptr(),
            out,
        )
    })
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect()
}

#[test]
fn a_host_configures_runs_and_manages_modules() {
    let host = Host::default();
    let component = common::rust_component("component-guest");
    let extism = common::rust_extism("extism-guest");
    let caps = json!({
        "hostCommands": ["echo"],
        "events": { "emit": ["test://*"], "listen": ["app://ping"] },
        "state": ["*"]
    });
    let backend = build(
        json!({
            "modules": {
                "fixture": { "path": component, "capabilities": caps },
                "stranger": { "path": extism },
                "asleep": { "path": extism, "enabled": false }
            },
            "limits": { "timeoutMs": 60000 }
        }),
        &host,
    );

    // What runs and what doesn't, and why.
    let modules = ok(|out| unsafe { wasm_backend_modules(backend, out) });
    assert_eq!(names(&modules), ["fixture"]);
    assert_eq!(modules[0]["capabilities"]["timeoutMs"], 60000);
    let inactive = ok(|out| unsafe { wasm_backend_inactive(backend, out) });
    assert_eq!(names(&inactive), ["asleep", "stranger"]);
    assert_eq!(inactive[0]["reason"], "disabled");
    assert_eq!(inactive[1]["reason"], "denied");
    assert!(
        inactive[1]["message"]
            .as_str()
            .unwrap()
            .contains("never seen before")
    );
    // The gate saw the hash before anything was compiled.
    let asked = host.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[1]["sha256"], inactive[1]["sha256"]);
    assert_eq!(asked[0]["reload"], false);

    // Commands: results are JSON, a module's own error is its value.
    let (status, text) = invoke(backend, "fixture", "add", json!({ "a": 2, "b": 40 }));
    assert_eq!((status, text.as_str()), (WasmBackendStatus::Ok, "42"));
    let too_old = json!({ "person": { "name": "Old", "ageYears": 200 } });
    let (status, text) = invoke(backend, "fixture", "describe", too_old);
    assert_eq!(status, WasmBackendStatus::Command);
    assert!(
        serde_json::from_str::<Value>(&text).unwrap().is_string(),
        "{text}"
    );
    let (status, text) = invoke(backend, "fixture", "nope", Value::Null);
    assert_eq!(status, WasmBackendStatus::CommandNotFound, "{text}");
    let (status, _) = invoke(backend, "nope", "add", Value::Null);
    assert_eq!(status, WasmBackendStatus::ModuleNotFound);
    let (status, _) = invoke(backend, "asleep", "greet", Value::Null);
    assert_eq!(status, WasmBackendStatus::Disabled);
    let (status, text) = invoke(backend, "fixture", "crash", Value::Null);
    assert_eq!(status, WasmBackendStatus::Runtime, "{text}");

    // Host commands reach the callback, and its answer reaches the module.
    let host_call = |args: &str| {
        invoke(
            backend,
            "fixture",
            "call-host",
            json!({ "command": "echo", "args": args }),
        )
    };
    let (status, text) = host_call("[1]");
    assert_eq!(status, WasmBackendStatus::Ok, "{text}");
    let echoed: String = serde_json::from_str(&text).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&echoed).unwrap(),
        json!({ "module": "fixture", "args": [1] })
    );
    let (status, text) = host_call("\"fail\"");
    assert_eq!(status, WasmBackendStatus::Command);
    assert!(text.contains("echo was asked to fail"), "{text}");
    let (_, text) = host_call("\"silent\"");
    assert!(text.contains("returned without replying"), "{text}");

    // Events: what modules emit reaches the host, what the host emits
    // reaches the modules listening to it.
    let (status, text) = invoke(
        backend,
        "fixture",
        "emit-event",
        json!({ "name": "test://hello", "payload": "{\"n\":1}" }),
    );
    assert_eq!(status, WasmBackendStatus::Ok, "{text}");
    {
        let events = host.events.lock().unwrap();
        assert!(events.contains(&("test://hello".to_string(), json!({ "n": 1 }))));
        assert!(events.iter().any(|(event, payload)| {
            event == "wasm-backend://module-loaded" && payload["name"] == "fixture"
        }));
        assert!(
            events
                .iter()
                .any(|(event, _)| event == "wasm-backend://module-trapped")
        );
    }
    ok(|out| unsafe {
        wasm_backend_emit(
            backend,
            c("app://ping").as_ptr(),
            c("{\"x\":1}").as_ptr(),
            out,
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_, last) = invoke(backend, "fixture", "last-event", Value::Null);
        if last != "null" {
            assert_eq!(last, r#""app://ping:{\"x\":1}""#);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "event was not delivered"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // State is readable and removable from outside the module.
    let remembered = json!({ "key": "theme", "value": "dark" });
    assert_eq!(
        invoke(backend, "fixture", "remember", remembered).0,
        WasmBackendStatus::Ok
    );
    let state = ok(|out| unsafe { wasm_backend_state(backend, c("fixture").as_ptr(), out) });
    assert_eq!(state, json!({ "theme": "dark" }));
    ok(|out| unsafe { wasm_backend_clear_state(backend, c("fixture").as_ptr(), out) });
    let state = ok(|out| unsafe { wasm_backend_state(backend, c("fixture").as_ptr(), out) });
    assert_eq!(state, json!({}));

    // Switching modules off and on.
    let info = ok(|out| unsafe { wasm_backend_enable(backend, c("asleep").as_ptr(), out) });
    assert_eq!(info["engine"], "extism");
    let (status, text) = invoke(backend, "asleep", "greet", json!({ "name": "C" }));
    assert_eq!(
        (status, text.as_str()),
        (WasmBackendStatus::Ok, "\"Hello, C!\"")
    );
    ok(|out| unsafe { wasm_backend_disable(backend, c("fixture").as_ptr(), out) });
    let (status, text) =
        call(|out| unsafe { wasm_backend_enable(backend, c("stranger").as_ptr(), out) });
    assert_eq!(status, WasmBackendStatus::NotAllowed, "{text}");
    let inactive = ok(|out| unsafe { wasm_backend_inactive(backend, out) });
    assert_eq!(names(&inactive), ["fixture", "stranger"]);

    // Loading at runtime, from memory and from a file.
    let bytes = std::fs::read(&extism).unwrap();
    let info = ok(|out| unsafe {
        wasm_backend_load_bytes(
            backend,
            c("embedded").as_ptr(),
            bytes.as_ptr(),
            bytes.len(),
            ptr::null(),
            out,
        )
    });
    assert_eq!(
        (&info["name"], &info["path"]),
        (&json!("embedded"), &Value::Null)
    );
    let config = c(&json!({ "path": extism }).to_string());
    let info =
        ok(|out| unsafe { wasm_backend_load(backend, c("file").as_ptr(), config.as_ptr(), out) });
    assert_eq!(info["sha256"], inactive[1]["sha256"]);
    let info = ok(|out| unsafe { wasm_backend_reload(backend, c("file").as_ptr(), out) });
    assert_eq!(info["generation"], 1);
    ok(|out| unsafe { wasm_backend_unload(backend, c("file").as_ptr(), out) });
    let (status, _) = call(|out| unsafe { wasm_backend_module(backend, c("file").as_ptr(), out) });
    assert_eq!(status, WasmBackendStatus::ModuleNotFound);
    let (status, source) =
        call(|out| unsafe { wasm_backend_typescript(backend, c("embedded").as_ptr(), out) });
    assert_eq!(status, WasmBackendStatus::Ok);
    assert!(source.contains("export const embedded"), "{source}");

    unsafe { wasm_backend_free(backend) };
}

#[test]
fn mistakes_are_statuses_not_crashes() {
    // Configuration typos are caught before anything is built.
    let mut builder = ptr::null_mut();
    let typo = c(r#"{ "module": {} }"#);
    let (status, text) =
        call(|out| unsafe { wasm_backend_builder_new(typo.as_ptr(), &mut builder, out) });
    assert_eq!(status, WasmBackendStatus::InvalidArgument);
    assert!(text.contains("unknown field `module`"), "{text}");
    assert!(builder.is_null());
    let not_json = c("{");
    let (status, text) =
        call(|out| unsafe { wasm_backend_builder_new(not_json.as_ptr(), &mut builder, out) });
    assert_eq!(status, WasmBackendStatus::InvalidArgument);
    assert!(text.contains("`config_json` is not JSON"), "{text}");

    // A builder can be dropped, and NULL is harmless everywhere.
    ok(|out| unsafe { wasm_backend_builder_new(ptr::null(), &mut builder, out) });
    let (status, text) = call(|out| unsafe {
        wasm_backend_builder_host_command(builder, c("x").as_ptr(), None, ptr::null_mut(), out)
    });
    assert_eq!(status, WasmBackendStatus::InvalidArgument);
    assert!(text.contains("`callback` is NULL"), "{text}");
    unsafe {
        wasm_backend_builder_free(builder);
        wasm_backend_builder_free(ptr::null_mut());
        wasm_backend_free(ptr::null_mut());
        wasm_backend_string_free(ptr::null_mut());
        wasm_backend_reply_ok(ptr::null_mut(), ptr::null());
    }
    let (status, text) = call(|out| unsafe { wasm_backend_modules(ptr::null(), out) });
    assert_eq!(status, WasmBackendStatus::InvalidArgument);
    assert!(text.contains("`backend` is NULL"), "{text}");
    let mut backend = ptr::null_mut();
    let (status, _) =
        call(|out| unsafe { wasm_backend_builder_build(ptr::null_mut(), &mut backend, out) });
    assert_eq!(status, WasmBackendStatus::InvalidArgument);

    // A backend without modules, and without caring about the text.
    ok(|out| unsafe { wasm_backend_builder_new(ptr::null(), &mut builder, out) });
    ok(|out| unsafe { wasm_backend_builder_build(builder, &mut backend, out) });
    let status = unsafe {
        wasm_backend_invoke(
            backend,
            c("nope").as_ptr(),
            c("x").as_ptr(),
            ptr::null(),
            ptr::null_mut(),
        )
    };
    assert_eq!(status, WasmBackendStatus::ModuleNotFound);
    let (status, text) = call(|out| unsafe {
        wasm_backend_invoke(backend, ptr::null(), c("x").as_ptr(), ptr::null(), out)
    });
    assert_eq!(status, WasmBackendStatus::InvalidArgument);
    assert!(text.contains("`module` is NULL"), "{text}");
    let bad = c("{ nope");
    let (status, _) =
        call(|out| unsafe { wasm_backend_load(backend, c("m").as_ptr(), bad.as_ptr(), out) });
    assert_eq!(status, WasmBackendStatus::InvalidArgument);
    // A path under `$APPDATA` needs to know where that is.
    let external = c(
        r#"{ "path": "$APPDATA/m.wasm", "capabilities": { "wasi": { "preopens": [{ "host": "data", "guest": "/data" }] } } }"#,
    );
    let (status, _) =
        call(|out| unsafe { wasm_backend_load(backend, c("m").as_ptr(), external.as_ptr(), out) });
    assert_ne!(status, WasmBackendStatus::Ok);
    unsafe { wasm_backend_free(backend) };

    let version = unsafe { CStr::from_ptr(wasm_backend_version()) }
        .to_str()
        .unwrap();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    let wit = unsafe { CStr::from_ptr(wasm_backend_wit()) }
        .to_str()
        .unwrap();
    assert!(wit.starts_with("package tauri:backend@"));
}
