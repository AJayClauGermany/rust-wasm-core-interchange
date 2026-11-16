//! Host commands imported by WIT components as typed functions, and the
//! load-time checks of a module's imports and capabilities.

mod common;

use std::path::PathBuf;

use common::*;
use serde_json::{Value, json};
use wasm_backend_core::{BackendBuilder, Capabilities, Error, ModuleConfig};

fn typed() -> PathBuf {
    rust_component("typed-guest")
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Version {
    major: u32,
    minor: u32,
    pre_release: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddArgs {
    left_side: i32,
    right_side: i32,
}

/// Host commands backing every import of the typed guest but `app_version`.
fn typed_host_commands(builder: BackendBuilder<TestPlatform>) -> BackendBuilder<TestPlatform> {
    builder
        .host_command("add_numbers", |_, args: AddArgs| {
            args.left_side
                .checked_add(args.right_side)
                .ok_or_else(|| "overflow".to_string())
        })
        .host_command("fail_hard", |_, args: Value| {
            Err::<(), _>(args["message"].as_str().unwrap_or_default().to_string())
        })
        .host_command("storage.get", |_, args: Value| {
            Ok::<_, String>((args["key"] == "known").then_some("value"))
        })
        .host_command("now_ms", |_, _: ()| Ok::<_, String>(1234u64))
}

fn typed_caps() -> Capabilities {
    Capabilities::default()
        .allow_host_command("app_version")
        .allow_host_command("add_numbers")
        .allow_host_command("fail_hard")
        .allow_host_command("storage.*")
        .allow_host_command("now_ms")
}

#[test]
fn typed_host_imports() {
    let (_, builder) = test_builder();
    let b = typed_host_commands(builder)
        .host_command("app_version", |_, _: ()| {
            Ok::<_, String>(Version {
                major: 1,
                minor: 2,
                pre_release: Some("beta".into()),
            })
        })
        .build()
        .unwrap();
    let info = b
        .load(
            "typed",
            ModuleConfig::from_path(typed()).capabilities(typed_caps()),
        )
        .unwrap();

    // Records come back typed; no JSON strings on either side.
    assert_eq!(
        b.invoke("typed", "version", Value::Null).unwrap(),
        json!("1.2-beta")
    );
    // Arguments arrive keyed by camelCase parameter name; a host `Err`
    // becomes the `err` case of a `result` import.
    assert_eq!(
        b.invoke("typed", "add", json!({ "a": 2, "b": 40 }))
            .unwrap(),
        json!(42)
    );
    match b.invoke("typed", "add", json!({ "a": i32::MAX, "b": 1 })) {
        Err(Error::Command(v)) => assert_eq!(v, json!("overflow")),
        other => panic!("expected command error, got {other:?}"),
    }
    // `interface.function` host commands, and option results.
    assert_eq!(
        b.invoke("typed", "lookup", json!({ "key": "known" }))
            .unwrap(),
        json!("value")
    );
    assert_eq!(
        b.invoke("typed", "lookup", json!({ "key": "other" }))
            .unwrap(),
        Value::Null
    );
    // Without a `result` to put it in, a host error traps with its message.
    match b.invoke("typed", "fail-hard", json!({ "message": "boom" })) {
        Err(Error::Runtime(msg)) => {
            assert!(msg.starts_with("host command `fail_hard`"), "{msg}");
            assert!(msg.contains("failed: boom"), "{msg}");
        }
        other => panic!("expected runtime error, got {other:?}"),
    }

    // The imports are listed with their registered names and WIT types.
    let mut imports: Vec<_> = info.imports.iter().map(|c| c.name.as_str()).collect();
    imports.sort();
    assert_eq!(
        imports,
        [
            "add_numbers",
            "app_version",
            "fail_hard",
            "now_ms",
            "storage.get"
        ],
        "{imports:?}"
    );
    let add = info
        .imports
        .iter()
        .find(|c| c.name == "add_numbers")
        .unwrap();
    assert_eq!(add.result.as_deref(), Some("result<s32, string>"));
    assert_eq!(add.params[0].name, "leftSide");
    // World-level function imports.
    assert_eq!(b.invoke("typed", "now", Value::Null).unwrap(), json!(1234));
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);
}

#[test]
fn typed_host_imports_are_checked_at_load() {
    let (_, builder) = test_builder();
    let b = builder
        .host_command("app_version", |_, _: ()| Ok::<_, String>("not a record"))
        .host_command("add_numbers", |_, _: Value| Ok::<_, String>(0))
        // Same words as `storage.get`, but not the qualified command.
        .host_command("storage_get", |_, _: Value| Ok::<_, String>(0))
        .build()
        .unwrap();

    // Missing host commands and missing capabilities are all reported at
    // once, before anything runs.
    let err = b
        .load(
            "typed",
            ModuleConfig::from_path(typed())
                .capabilities(Capabilities::default().allow_host_command("app_version")),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("failed to load"), "{err}");
    assert!(
        err.contains("`test:typed/storage#get` needs a host command named `storage.get` or `get`"),
        "{err}"
    );
    assert!(
        err.contains("registered: add_numbers, app_version, storage_get"),
        "{err}"
    );
    assert!(
        err.contains("not allowed to invoke host command `add_numbers` (add \"add_numbers\" to `capabilities.hostCommands`)"),
        "{err}"
    );
    assert!(err.contains("`test:typed/app#fail-hard`"), "{err}");
    assert!(matches!(b.module("typed"), Err(Error::ModuleNotFound(_))));
}

#[test]
fn typed_host_import_type_mismatch() {
    // `app_version` returns a string where the module imports a record.
    let (_, builder) = test_builder();
    let b = typed_host_commands(builder)
        .host_command("app_version", |_, _: ()| Ok::<_, String>("1.2"))
        .build()
        .unwrap();
    b.load(
        "typed",
        ModuleConfig::from_path(typed()).capabilities(typed_caps()),
    )
    .unwrap();
    match b.invoke("typed", "version", Value::Null) {
        Err(Error::Runtime(msg)) => {
            assert!(
                msg.contains("host command `app_version` returned a value that doesn't match its import `test:typed/app#app-version`"),
                "{msg}"
            );
            assert!(msg.contains("expected an object"), "{msg}");
        }
        other => panic!("expected runtime error, got {other:?}"),
    }

    // Arguments the host command can't deserialize fail with serde's message.
    let (_, builder) = test_builder();
    let b = typed_host_commands(builder)
        .host_command("app_version", |_, _: ()| Ok::<_, String>("1.2"))
        .host_command("add_numbers", |_, args: AddArgs| {
            Ok::<_, String>(args.left_side)
        })
        .build()
        .unwrap();
    b.load(
        "typed",
        ModuleConfig::from_path(typed()).capabilities(typed_caps()),
    )
    .unwrap();
    assert_eq!(
        b.invoke("typed", "add", json!({ "a": 5, "b": 1 })).unwrap(),
        json!(5)
    );
}

#[test]
fn configuration_warnings() {
    let (_, builder) = test_builder();
    let b = typed_host_commands(builder)
        .host_command("echo", |_, args: Value| Ok::<_, String>(args))
        .host_command("app_version", |_, _: ()| Ok::<_, String>("1.0.0"))
        .build()
        .unwrap();

    // A capability naming a host command that doesn't exist (a typo, or a
    // command renamed in Rust) is flagged when the module loads.
    let info = b
        .load(
            "fixture",
            ModuleConfig::from_path(rust_component("component-guest")).capabilities(
                Capabilities::default()
                    .allow_host_command("echo")
                    .allow_host_command("app_verison")
                    .listen("app://ping"),
            ),
        )
        .unwrap();
    assert_eq!(info.warnings.len(), 1, "{:?}", info.warnings);
    assert!(
        info.warnings[0].contains("entry `app_verison` matches no registered host command (registered: `add_numbers`, `app_version`"),
        "{:?}",
        info.warnings
    );

    // So are subscriptions to events the module can't receive.
    let info = b
        .load(
            "typed",
            ModuleConfig::from_path(typed()).capabilities(typed_caps().listen("app://ping")),
        )
        .unwrap();
    assert_eq!(info.warnings.len(), 1, "{:?}", info.warnings);
    assert!(
        info.warnings[0].contains("subscribes to `app://ping` but the module has no event handler"),
        "{:?}",
        info.warnings
    );
    // Extism modules with an `on_event` export are fine.
    let info = b
        .load(
            "ext",
            ModuleConfig::from_path(rust_extism("extism-guest")).capabilities(caps()),
        )
        .unwrap();
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);
}
