//! What the runtime does around a module's own code: counting traps and
//! stopping a module that keeps trapping, asking the load gate, and running
//! WIT guests that were built without WASI.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use serde_json::{Value, json};
use wasm_backend_core::{EVENT_MODULE_TRAPPED, EngineKind, Error, ModuleConfig};

fn trap_events(platform: &TestPlatform) -> Vec<Value> {
    platform
        .emitted
        .lock()
        .unwrap()
        .iter()
        .filter(|(event, _)| event == EVENT_MODULE_TRAPPED)
        .map(|(_, payload)| payload.clone())
        .collect()
}

#[test]
fn a_module_that_keeps_trapping_is_stopped_until_reloaded() {
    let (platform, b) = backend();
    b.load(
        "fixture",
        ModuleConfig::from_path(rust_component("component-guest"))
            .capabilities(caps().max_restarts(1)),
    )
    .unwrap();
    let count = || b.invoke("fixture", "count", Value::Null);
    let crash = || b.invoke("fixture", "crash", Value::Null);
    assert_eq!(count().unwrap(), json!(1));

    // The first trap restarts the module from fresh memory.
    assert!(matches!(crash(), Err(Error::Runtime(_))));
    let info = b.module("fixture").unwrap();
    assert_eq!((info.traps, info.stopped), (1, false));
    assert_eq!(count().unwrap(), json!(1));

    // An error the module returns is not a trap.
    let too_old = json!({ "person": { "name": "Old", "ageYears": 200 } });
    assert!(matches!(
        b.invoke("fixture", "describe", too_old),
        Err(Error::Command(_))
    ));
    assert_eq!(b.module("fixture").unwrap().traps, 1);

    // The trap after the last restart stops it: nothing runs any more.
    assert!(matches!(crash(), Err(Error::Runtime(_))));
    let info = b.module("fixture").unwrap();
    assert_eq!((info.traps, info.stopped), (2, true));
    match count() {
        Err(Error::Stopped { module, traps }) => {
            assert_eq!((module.as_str(), traps), ("fixture", 2))
        }
        other => panic!("expected a stopped module, got {other:?}"),
    }
    assert!(matches!(
        b.send_event("fixture", "app://ping", &Value::Null),
        Err(Error::Stopped { .. })
    ));

    let events = trap_events(&platform);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["module"], "fixture");
    assert_eq!(
        (&events[0]["traps"], &events[0]["stopped"]),
        (&json!(1), &json!(false))
    );
    assert_eq!(
        (&events[1]["traps"], &events[1]["stopped"]),
        (&json!(2), &json!(true))
    );
    assert!(
        events[1]["error"]
            .as_str()
            .unwrap()
            .contains("WASM runtime error")
    );

    // Reloading starts it again, with its restarts back.
    let info = b.reload("fixture").unwrap();
    assert_eq!((info.traps, info.stopped), (0, false));
    assert_eq!(count().unwrap(), json!(1));
}

#[test]
fn traps_are_counted_without_a_limit() {
    let (platform, b) = backend();
    b.load(
        "ext",
        ModuleConfig::from_path(rust_extism("extism-guest")).capabilities(caps()),
    )
    .unwrap();
    for traps in 1..=3 {
        assert!(matches!(
            b.invoke("ext", "crash", Value::Null),
            Err(Error::Runtime(_))
        ));
        let info = b.module("ext").unwrap();
        assert_eq!((info.traps, info.stopped), (traps, false));
    }
    assert_eq!(trap_events(&platform).len(), 3);
    assert_eq!(
        b.invoke("ext", "greet", json!({ "name": "again" }))
            .unwrap(),
        json!("Hello, again!")
    );
}

#[test]
fn the_load_gate_decides_what_runs() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let refuse_reloads = Arc::new(Mutex::new(false));
    let (_, builder) = test_builder();
    let b = builder
        .load_gate({
            let (seen, refuse_reloads) = (seen.clone(), refuse_reloads.clone());
            move |_platform, request| {
                seen.lock().unwrap().push((
                    request.name.to_string(),
                    request.sha256.to_string(),
                    request.reload,
                ));
                if request.name == "unknown" {
                    return Err("not on the list".to_string());
                }
                if request.reload && *refuse_reloads.lock().unwrap() {
                    return Err("changed since it was approved".to_string());
                }
                Ok(())
            }
        })
        .build()
        .unwrap();
    let path = rust_component("component-guest");

    let info = b.load("fixture", ModuleConfig::from_path(&path)).unwrap();
    assert_eq!(info.sha256.len(), 64);
    assert!(info.sha256.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(
        *seen.lock().unwrap(),
        [("fixture".to_string(), info.sha256.clone(), false)]
    );

    // A refused module is never compiled, let alone run.
    match b.load(
        "unknown",
        ModuleConfig::from_bytes(b"not even wasm".to_vec()),
    ) {
        Err(Error::NotAllowed(msg)) => {
            assert!(
                msg.contains("`unknown`") && msg.contains("not on the list"),
                "{msg}"
            )
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(b.module("unknown").is_err());

    // Reloads are asked too, and a refused one keeps the old version.
    assert_eq!(b.invoke("fixture", "count", Value::Null).unwrap(), json!(1));
    *refuse_reloads.lock().unwrap() = true;
    assert!(matches!(b.reload("fixture"), Err(Error::NotAllowed(_))));
    assert!(seen.lock().unwrap().last().unwrap().2);
    assert_eq!(b.invoke("fixture", "count", Value::Null).unwrap(), json!(2));
    assert_eq!(b.module("fixture").unwrap().generation, 0);
}

#[test]
fn a_wit_guest_built_without_wasi() {
    let (_, b) = backend();
    let path = rust_guest("bare-guest", "wasm32-unknown-unknown");
    let info = b
        .load(
            "bare",
            ModuleConfig::from_path(&path).capabilities(caps().max_restarts(0)),
        )
        .unwrap();

    // A core module, detected as a WIT guest and wrapped into a component.
    assert_eq!(&std::fs::read(&path).unwrap()[4..8], [1, 0, 0, 0]);
    assert_eq!(info.engine, EngineKind::Component);
    // All it can reach is the host interface: there is no WASI to fall back on.
    assert_eq!(info.requires, ["tauri:backend/host@0.1.0"]);
    let commands: Vec<&str> = info.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(commands, ["greet", "count", "call-host", "crash"]);

    assert_eq!(
        b.invoke("bare", "greet", json!({ "name": "bare" }))
            .unwrap(),
        json!("Hello, bare!")
    );
    assert_eq!(b.invoke("bare", "count", Value::Null).unwrap(), json!(1));
    assert_eq!(b.invoke("bare", "count", Value::Null).unwrap(), json!(2));
    let echoed = b
        .invoke(
            "bare",
            "call-host",
            json!({ "command": "echo", "args": "[1]" }),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(echoed.as_str().unwrap()).unwrap(),
        json!({ "module": "bare", "args": [1] })
    );

    // A panic is a trap here as well, and with no restarts it is the last.
    assert!(matches!(
        b.invoke("bare", "crash", Value::Null),
        Err(Error::Runtime(_))
    ));
    assert!(matches!(
        b.invoke("bare", "count", Value::Null),
        Err(Error::Stopped { .. })
    ));

    // WASI guests list what they import the same way.
    let wasi = b
        .load(
            "fixture",
            ModuleConfig::from_path(rust_component("component-guest")),
        )
        .unwrap();
    assert!(
        wasi.requires.iter().any(|i| i.starts_with("wasi:")),
        "{:?}",
        wasi.requires
    );
    assert!(
        wasi.requires
            .iter()
            .any(|i| i == "tauri:backend/host@0.1.0")
    );

    // A core module that is neither is told so by the engine it was given to.
    let extism = std::fs::read(rust_extism("extism-guest")).unwrap();
    match b.load(
        "wrong",
        ModuleConfig::from_bytes(extism).engine(EngineKind::Component),
    ) {
        Err(Error::Load { message, .. }) => {
            assert!(
                message.contains("can't be made into a component"),
                "{message}"
            )
        }
        other => panic!("expected a load error, got {other:?}"),
    }
}
