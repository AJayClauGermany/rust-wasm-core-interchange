//! The per-module state store: what a module keeps through
//! `tauri:backend/state` (`tauri_state_*` for Extism) outlives its memory.

mod common;

use std::{collections::BTreeMap, path::PathBuf};

use common::*;
use serde_json::{Value, json};
use wasm_backend_core::{
    Backend, Capabilities, Error, FileStateStore, ModuleConfig, STATE_KEY_MAX, STATE_VALUE_MAX,
};

fn component(caps: Capabilities) -> ModuleConfig {
    ModuleConfig::from_path(rust_component("component-guest")).capabilities(caps)
}

fn scratch(name: &str) -> PathBuf {
    let dir = repo().join("target/test-tmp").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn err_text(result: wasm_backend_core::Result<Value>) -> String {
    match result {
        Err(Error::Command(Value::String(message))) => message,
        other => panic!("expected the module to return an error, got {other:?}"),
    }
}

#[test]
fn state_outlives_the_modules_memory() {
    let (_, b) = backend();
    b.load("fixture", component(caps().allow_state("*")))
        .unwrap();
    let visits = || b.invoke("fixture", "visits", Value::Null).unwrap();
    let recall = |key: &str| {
        b.invoke("fixture", "recall", json!({ "key": key }))
            .unwrap()
    };

    assert_eq!(recall("color"), Value::Null);
    b.invoke(
        "fixture",
        "remember",
        json!({ "key": "color", "value": "teal" }),
    )
    .unwrap();
    assert_eq!(recall("color"), json!("teal"));
    assert_eq!(visits(), json!(1));
    assert_eq!(visits(), json!(2));
    assert_eq!(
        b.invoke("fixture", "remembered", Value::Null).unwrap(),
        json!(["color", "visits"])
    );

    // A trap resets the module's memory, and drops what that call wrote;
    // everything stored before it is still there.
    assert_eq!(b.invoke("fixture", "count", Value::Null).unwrap(), json!(1));
    assert!(matches!(
        b.invoke(
            "fixture",
            "remember-then-crash",
            json!({ "key": "color", "value": "red" })
        ),
        Err(Error::Runtime(_))
    ));
    assert_eq!(b.invoke("fixture", "count", Value::Null).unwrap(), json!(1));
    assert_eq!(recall("color"), json!("teal"));
    assert_eq!(visits(), json!(3));

    // So does a reload, and unloading and loading the module again.
    b.reload("fixture").unwrap();
    assert_eq!(visits(), json!(4));
    b.unload("fixture").unwrap();
    assert_eq!(b.state("fixture").unwrap()["visits"], "4");
    b.load("fixture", component(caps().allow_state("*")))
        .unwrap();
    assert_eq!(visits(), json!(5));

    b.invoke("fixture", "forget", json!({ "key": "color" }))
        .unwrap();
    assert_eq!(recall("color"), Value::Null);
    assert_eq!(
        b.state("fixture").unwrap(),
        BTreeMap::from([("visits".to_string(), "5".to_string())])
    );

    // The host can wipe it, e.g. when a plugin is uninstalled.
    b.clear_state("fixture").unwrap();
    assert_eq!(visits(), json!(1));
    // Each module has a store of its own.
    assert!(b.state("other").unwrap().is_empty());
}

#[test]
fn state_is_a_capability_scoped_by_key() {
    let (_, b) = backend();

    // Without the capability every call is refused, and the load says so.
    let info = b.load("none", component(caps())).unwrap();
    assert!(
        info.warnings
            .iter()
            .any(|w| w.contains("`capabilities.state` grants no keys")),
        "{:?}",
        info.warnings
    );
    let err = err_text(b.invoke("none", "recall", json!({ "key": "color" })));
    assert!(
        err.contains("not allowed to use state key `color`"),
        "{err}"
    );
    let err = err_text(b.invoke("none", "remember", json!({ "key": "color", "value": "x" })));
    assert!(err.contains("not allowed"), "{err}");
    assert!(b.state("none").unwrap().is_empty());

    let info = b
        .load(
            "scoped",
            component(caps().allow_state("prefs/*").allow_state("visits")),
        )
        .unwrap();
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);
    let remember = |key: &str, value: &str| {
        b.invoke("scoped", "remember", json!({ "key": key, "value": value }))
    };
    remember("prefs/theme", "dark").unwrap();
    assert!(err_text(remember("secrets/token", "x")).contains("not allowed"));
    assert!(
        err_text(b.invoke("scoped", "forget", json!({ "key": "other" }))).contains("not allowed")
    );
    b.invoke("scoped", "visits", Value::Null).unwrap();
    assert_eq!(
        b.invoke("scoped", "remembered", Value::Null).unwrap(),
        json!(["prefs/theme", "visits"])
    );

    // Keys and values have a size limit.
    let long_key = format!("prefs/{}", "k".repeat(STATE_KEY_MAX));
    assert!(err_text(remember(&long_key, "x")).contains("at most 256 bytes"));
    assert!(
        err_text(remember("prefs/big", &"v".repeat(STATE_VALUE_MAX + 1)))
            .contains("at most 65536 bytes")
    );
    remember("prefs/big", &"v".repeat(STATE_VALUE_MAX)).unwrap();
}

#[test]
fn extism_modules_have_state_too() {
    let (_, b) = backend();
    b.load(
        "ext",
        ModuleConfig::from_path(rust_extism("extism-guest"))
            .capabilities(caps().allow_state("notes/*")),
    )
    .unwrap();
    let call = |command: &str, args: Value| b.invoke("ext", command, args);

    assert_eq!(
        call("recall", json!({ "key": "notes/a" })).unwrap(),
        Value::Null
    );
    call("remember", json!({ "key": "notes/a", "value": "first" })).unwrap();
    call("remember", json!({ "key": "notes/b", "value": "second" })).unwrap();
    assert_eq!(
        call("recall", json!({ "key": "notes/a" })).unwrap(),
        json!("first")
    );
    assert_eq!(
        call("remembered", Value::Null).unwrap(),
        json!(["notes/a", "notes/b"])
    );

    let err = call("remember", json!({ "key": "elsewhere", "value": "x" })).unwrap_err();
    assert!(
        err.to_string().contains("not allowed to use state key"),
        "{err}"
    );

    assert!(matches!(
        call(
            "remember_then_crash",
            json!({ "key": "notes/a", "value": "lost" })
        ),
        Err(Error::Runtime(_))
    ));
    assert_eq!(
        call("recall", json!({ "key": "notes/a" })).unwrap(),
        json!("first")
    );

    call("forget", json!({ "key": "notes/b" })).unwrap();
    assert_eq!(
        b.state("ext").unwrap(),
        BTreeMap::from([("notes/a".to_string(), "first".to_string())])
    );
}

#[test]
fn a_state_store_keeps_state_between_backends() {
    let dir = scratch("state-store");
    let start = || -> Backend<TestPlatform> {
        let (_, builder) = test_builder();
        let b = builder
            .state_store(FileStateStore::new(&dir))
            .build()
            .unwrap();
        b.load(
            "fixture",
            component(Capabilities::default().allow_state("*")),
        )
        .unwrap();
        b
    };

    let first = start();
    // Nothing is written until a module stores something.
    assert!(!dir.exists());
    assert_eq!(
        first.invoke("fixture", "visits", Value::Null).unwrap(),
        json!(1)
    );
    assert_eq!(
        first.invoke("fixture", "visits", Value::Null).unwrap(),
        json!(2)
    );
    let file = FileStateStore::new(&dir).path("fixture");
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&file).unwrap()).unwrap(),
        json!({ "visits": "2" })
    );
    drop(first);

    // A new backend, as after restarting the app, picks up where it left off.
    let second = start();
    assert_eq!(
        second.invoke("fixture", "visits", Value::Null).unwrap(),
        json!(3)
    );
    second.clear_state("fixture").unwrap();
    assert!(!file.exists());
}
