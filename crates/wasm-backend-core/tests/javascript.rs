//! A WIT component written in JavaScript (`guests/javascript/greeter`).

mod common;

use std::{sync::mpsc, time::Duration};

use common::*;
use serde_json::json;
use wasm_backend_core::{Capabilities, Error, ModuleConfig, Platform};

#[test]
fn javascript_component() {
    let Some(path) = js_component("greeter") else {
        return;
    };
    let (platform, builder) = test_builder();
    let b = builder
        .host_command("app_version", |_, _: ()| Ok::<_, String>("1.0.0"))
        .build()
        .unwrap();
    let caps = Capabilities::default()
        .allow_emit("greeter://*")
        .listen("app://ping");

    // It imports the `app_version` host command, so it needs the capability.
    let err = b
        .load(
            "greeter",
            ModuleConfig::from_path(&path).capabilities(caps.clone()),
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not allowed to invoke host command `app_version`"),
        "{err}"
    );

    let info = b
        .load(
            "greeter",
            ModuleConfig::from_path(&path).capabilities(caps.allow_host_command("app_version")),
        )
        .unwrap();
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);

    let (tx, rx) = mpsc::channel();
    platform
        .bus
        .listen("greeter://greeted", Box::new(move |v| tx.send(v).unwrap()));
    let out = b
        .invoke("greeter", "greet", json!({ "name": "JS" }))
        .unwrap();
    assert!(out.as_str().unwrap().contains("JS"), "{out}");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        json!({ "name": "JS" })
    );

    // Records arrive with camelCase keys; a thrown string becomes `err`.
    assert_eq!(
        b.invoke(
            "greeter",
            "describe",
            json!({ "person": { "name": "Ada", "ageYears": 36 } }),
        )
        .unwrap(),
        json!("Ada is 36 years old (app v1.0.0)")
    );
    match b.invoke(
        "greeter",
        "describe",
        json!({ "person": { "name": "", "ageYears": 1 } }),
    ) {
        Err(Error::Command(v)) => assert_eq!(v, json!("a person needs a name")),
        other => panic!("expected command error, got {other:?}"),
    }

    // Module state is kept between calls.
    assert_eq!(
        b.invoke("greeter", "increment", json!({ "by": 2 }))
            .unwrap(),
        json!(2)
    );
    assert_eq!(
        b.invoke("greeter", "increment", json!({ "by": 3 }))
            .unwrap(),
        json!(5)
    );

    // Subscribed events reach the module's `on-event` export.
    let (tx, rx) = mpsc::channel();
    platform
        .bus
        .listen("greeter://pong", Box::new(move |v| tx.send(v).unwrap()));
    platform.emit("app://ping", json!({ "n": 1 })).unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        json!({ "count": 5, "received": { "n": 1 } })
    );
}
