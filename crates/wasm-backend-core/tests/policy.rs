//! Deciding what runs: switching modules off and on, the lists and
//! per-plugin settings of external plugins, pinned hashes, shared limits,
//! and seeing everything that is not running in `Backend::inactive`.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use serde_json::{Value, json};
use wasm_backend_core::{
    Capabilities, Error, ExternalConfig, ExternalPluginConfig, InactiveReason, Limits, ModuleConfig,
};

/// An empty directory under the system's temporary one.
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wasm-core-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn names<T>(items: &[T], name: impl Fn(&T) -> &str) -> Vec<String> {
    items.iter().map(|i| name(i).to_string()).collect()
}

#[test]
fn modules_can_be_switched_off_and_on() {
    let (_, b) = backend();
    let path = rust_extism("extism-guest");
    b.load_all([
        (
            "on".to_string(),
            ModuleConfig::from_path(&path).capabilities(caps()),
        ),
        (
            "off".to_string(),
            ModuleConfig::from_path(&path).enabled(false),
        ),
    ]);
    assert_eq!(names(&b.modules(), |m| &m.name), ["on"]);

    // A module that is configured but switched off is listed, not loaded.
    let inactive = b.inactive();
    assert_eq!(inactive.len(), 1);
    assert_eq!(inactive[0].name, "off");
    assert_eq!(inactive[0].reason, InactiveReason::Disabled);
    assert_eq!(inactive[0].path.as_deref(), Some(path.as_path()));
    assert!(matches!(
        b.invoke("off", "greet", json!({ "name": "x" })),
        Err(Error::Disabled(_))
    ));

    let info = b.enable("off").unwrap();
    assert_eq!(info.name, "off");
    assert!(b.inactive().is_empty());
    assert_eq!(
        b.invoke("off", "greet", json!({ "name": "x" })).unwrap(),
        json!("Hello, x!")
    );
    // Enabling what already runs changes nothing.
    assert_eq!(b.enable("off").unwrap().generation, 0);

    // Disabling unloads, and keeps what is needed to load it again.
    b.disable("on").unwrap();
    assert_eq!(names(&b.modules(), |m| &m.name), ["off"]);
    let inactive = b.inactive();
    assert_eq!(inactive[0].name, "on");
    assert_eq!(inactive[0].reason, InactiveReason::Disabled);
    assert_eq!(inactive[0].sha256.as_deref().map(str::len), Some(64));
    assert_eq!(inactive[0].capabilities, caps());
    b.disable("on").unwrap();
    assert!(b.enable("on").is_ok());

    // Unloading forgets a module altogether.
    b.disable("on").unwrap();
    b.unload("on").unwrap();
    assert!(b.inactive().is_empty());
    assert!(matches!(b.enable("on"), Err(Error::ModuleNotFound(_))));
    assert!(matches!(b.disable("nope"), Err(Error::ModuleNotFound(_))));
}

#[test]
fn refused_and_failed_modules_are_listed_with_the_reason() {
    let approved = Arc::new(Mutex::new(false));
    let (_, builder) = test_builder();
    let b = builder
        .load_gate({
            let approved = approved.clone();
            move |_, request| match request.name {
                "plugin" if !*approved.lock().unwrap() => Err("not approved yet".to_string()),
                _ => Ok(()),
            }
        })
        .build()
        .unwrap();
    let path = rust_extism("extism-guest");
    let caps = Capabilities::default().allow_emit("plugin://*");
    b.load_all([
        (
            "plugin".to_string(),
            ModuleConfig::from_path(&path).capabilities(caps.clone()),
        ),
        (
            "missing".to_string(),
            ModuleConfig::from_path(path.with_file_name("nope.wasm")),
        ),
    ]);
    assert!(b.modules().is_empty());

    let inactive = b.inactive();
    assert_eq!(names(&inactive, |m| &m.name), ["missing", "plugin"]);
    let (missing, plugin) = (&inactive[0], &inactive[1]);
    assert_eq!(missing.reason, InactiveReason::Failed);
    assert_eq!(missing.sha256, None);
    assert!(
        missing.message.contains("can't read"),
        "{}",
        missing.message
    );
    // A refused module shows what a user would be asked to approve.
    assert_eq!(plugin.reason, InactiveReason::Denied);
    assert!(
        plugin.message.contains("not approved yet"),
        "{}",
        plugin.message
    );
    assert_eq!(plugin.sha256.as_deref().map(str::len), Some(64));
    assert_eq!(plugin.capabilities, caps);
    assert!(matches!(
        b.invoke("plugin", "count", Value::Null),
        Err(Error::ModuleNotFound(_))
    ));

    // Still refused: it stays listed. Approved: `enable` loads it.
    assert!(matches!(b.enable("plugin"), Err(Error::NotAllowed(_))));
    assert_eq!(b.inactive().len(), 2);
    *approved.lock().unwrap() = true;
    assert_eq!(
        b.enable("plugin").unwrap().sha256,
        plugin.sha256.clone().unwrap()
    );
    assert_eq!(names(&b.inactive(), |m| &m.name), ["missing"]);
}

#[test]
fn a_pinned_hash_only_loads_those_bytes() {
    let (_, b) = backend();
    let path = rust_extism("extism-guest");
    let sha256 = b
        .load("probe", ModuleConfig::from_path(&path))
        .unwrap()
        .sha256;

    let pinned = ModuleConfig::from_path(&path).sha256(sha256.to_uppercase());
    assert!(b.load("pinned", pinned).is_ok());

    let other = ModuleConfig::from_path(&path).sha256("00".repeat(32));
    match b.load("other", other) {
        Err(Error::NotAllowed(message)) => {
            assert!(
                message.contains(&sha256) && message.contains("`sha256`"),
                "{message}"
            )
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let inactive = b.inactive();
    assert_eq!(inactive[0].reason, InactiveReason::Denied);
    assert_eq!(inactive[0].sha256.as_deref(), Some(sha256.as_str()));
}

#[test]
fn external_plugins_are_chosen_and_configured_by_name() {
    let dir = temp_dir("external");
    let guest = rust_extism("extism-guest");
    for name in ["weather", "notes", "legacy-sync", "sleeper"] {
        std::fs::copy(&guest, dir.join(format!("{name}.wasm"))).unwrap();
    }
    let (_, builder) = test_builder();
    let b = builder
        .external(Some(
            ExternalConfig::new(&dir)
                .capabilities(Capabilities::default().allow_emit("plugin://*"))
                .deny("legacy-*")
                .plugin(
                    "weather",
                    ExternalPluginConfig::default()
                        .capabilities(Capabilities::default().allow_emit("weather://*"))
                        .expose(true),
                )
                .plugin("sleeper", ExternalPluginConfig::default().enabled(false)),
        ))
        .build()
        .unwrap();
    b.load_all([]);

    let modules = b.modules();
    assert_eq!(names(&modules, |m| &m.name), ["notes", "weather"]);
    let (notes, weather) = (&modules[0], &modules[1]);
    assert!(notes.external && !notes.exposed);
    assert_eq!(notes.capabilities.events.emit, ["plugin://*"]);
    // One plugin's settings replace the shared ones.
    assert!(weather.exposed);
    assert_eq!(weather.capabilities.events.emit, ["weather://*"]);
    let emit = |module: &str, event: &str| {
        b.invoke(
            module,
            "emit_event",
            json!({ "event": event, "payload": 1 }),
        )
    };
    assert!(emit("weather", "weather://rain").is_ok());
    assert!(emit("weather", "plugin://x").is_err());
    assert!(emit("notes", "plugin://x").is_ok());

    let inactive = b.inactive();
    assert_eq!(names(&inactive, |m| &m.name), ["legacy-sync", "sleeper"]);
    assert_eq!(inactive[0].reason, InactiveReason::Denied);
    assert!(
        inactive[0].message.contains("`legacy-*`"),
        "{}",
        inactive[0].message
    );
    assert_eq!(inactive[0].sha256.as_deref().map(str::len), Some(64));
    assert_eq!(inactive[1].reason, InactiveReason::Disabled);

    // Scanning again leaves switched-off and refused plugins where they are.
    b.disable("notes").unwrap();
    b.sync_external();
    assert_eq!(names(&b.modules(), |m| &m.name), ["weather"]);
    assert_eq!(b.inactive().len(), 3);
    // A refused plugin is not loaded by `enable` either.
    assert!(b.enable("sleeper").is_ok());
    assert!(b.enable("notes").is_ok());

    // A plugin whose file is gone is forgotten, wherever it was listed.
    std::fs::remove_file(dir.join("legacy-sync.wasm")).unwrap();
    std::fs::remove_file(dir.join("weather.wasm")).unwrap();
    b.sync_external();
    assert_eq!(names(&b.modules(), |m| &m.name), ["notes", "sleeper"]);
    assert!(b.inactive().is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_allow_list_keeps_other_plugins_out() {
    let dir = temp_dir("allow");
    let guest = rust_extism("extism-guest");
    for name in ["official-notes", "stranger"] {
        std::fs::copy(&guest, dir.join(format!("{name}.wasm"))).unwrap();
    }
    let (_, builder) = test_builder();
    let b = builder
        .external(Some(ExternalConfig::new(&dir).allow("official-*")))
        .build()
        .unwrap();
    b.load_all([]);
    assert_eq!(names(&b.modules(), |m| &m.name), ["official-notes"]);
    let inactive = b.inactive();
    assert_eq!(names(&inactive, |m| &m.name), ["stranger"]);
    assert!(inactive[0].message.contains("external.allow"));
    // The lists are the app's decision: `enable` doesn't get around them.
    assert!(b.enable("stranger").is_err());
    assert!(b.module("stranger").is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn limits_apply_to_every_module_without_its_own() {
    let (_, builder) = test_builder();
    let b = builder
        .limits(Limits::default().timeout_ms(60_000).max_restarts(0))
        .build()
        .unwrap();
    let path = rust_component("component-guest");
    let info = b.load("shared", ModuleConfig::from_path(&path)).unwrap();
    assert_eq!(info.capabilities.timeout_ms, Some(60_000));
    assert_eq!(info.capabilities.max_restarts, Some(0));
    // With no restarts allowed, the first trap stops it.
    assert!(b.invoke("shared", "crash", Value::Null).is_err());
    assert!(matches!(
        b.invoke("shared", "count", Value::Null),
        Err(Error::Stopped { .. })
    ));

    let own = ModuleConfig::from_path(&path)
        .capabilities(Capabilities::default().timeout_ms(500).max_restarts(5));
    let info = b.load("own", own).unwrap();
    assert_eq!(info.capabilities.timeout_ms, Some(500));
    assert_eq!(info.capabilities.max_restarts, Some(5));
}

#[test]
fn deny_entries_take_back_what_wildcards_grant() {
    let (_, builder) = test_builder();
    let b = builder
        .host_command("db_query", |_, _: Value| Ok::<_, String>("rows"))
        .host_command("db_drop", |_, _: Value| Ok::<_, String>("gone"))
        .build()
        .unwrap();
    let info = b
        .load(
            "ext",
            ModuleConfig::from_path(rust_extism("extism-guest")).capabilities(
                Capabilities::default()
                    .allow_host_command("db_*")
                    .deny_host_command("db_drop")
                    .allow_emit("*")
                    .deny_emit("admin://*"),
            ),
        )
        .unwrap();
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);
    let call = |command: &str| {
        b.invoke(
            "ext",
            "call_host",
            json!({ "command": command, "args": null }),
        )
    };
    assert_eq!(call("db_query").unwrap(), json!("rows"));
    let refused = call("dbDrop").unwrap_err().to_string();
    assert!(refused.contains("not allowed"), "{refused}");
    let emit = |event: &str| b.invoke("ext", "emit_event", json!({ "event": event, "payload": 1 }));
    assert!(emit("app://saved").is_ok());
    assert!(emit("admin://reset").is_err());
}
