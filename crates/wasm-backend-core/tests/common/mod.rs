//! Shared harness for the integration tests: a minimal, framework-free
//! [`Platform`] and builders for the guest modules in `guests/`.
//!
//! Rust guests are built on demand with `cargo`, which needs the
//! `wasm32-wasip2` (WIT components) and `wasm32-unknown-unknown` (Extism)
//! targets. JavaScript guests are built with `node` and need
//! `npm ci` in `guests/javascript` first; without it their tests are skipped,
//! unless `WASM_CORE_REQUIRE_ALL_GUESTS` is set (as in CI).

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use wasm_backend_core::{
    Backend, BackendBuilder, BaseDir, Capabilities, EventBus, EventHandler, Platform, Result,
    TypeScriptOptions,
};

/// Root of the repository.
pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Directory of a guest: `guests/<language>/<name>`.
pub fn guest_dir(language: &str, name: &str) -> PathBuf {
    repo().join("guests").join(language).join(name)
}

/// Serializes guest builds across the tests of one binary.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

/// Put the host's WIT package where a guest's `wit/` folder expects it.
fn install_wit_deps(dir: &Path) {
    if dir.join("wit").exists() {
        let deps = dir.join("wit/deps/tauri-backend");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join("backend.wit"), wasm_backend_core::WIT).unwrap();
    }
}

/// Build a Rust guest from `guests/rust` for `target` and return its `.wasm`.
pub fn rust_guest(name: &str, target: &str) -> PathBuf {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = guest_dir("rust", name);
    let target_dir = repo().join("target/guests");
    install_wit_deps(&dir);
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--target", target, "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("failed to run cargo");
    assert!(status.success(), "failed to build guest {name}");
    target_dir
        .join(target)
        .join("release")
        .join(format!("{}.wasm", name.replace('-', "_")))
}

/// A WIT component written in Rust (`wasm32-wasip2`).
pub fn rust_component(name: &str) -> PathBuf {
    rust_guest(name, "wasm32-wasip2")
}

/// An Extism plugin written in Rust (`wasm32-unknown-unknown`).
pub fn rust_extism(name: &str) -> PathBuf {
    rust_guest(name, "wasm32-unknown-unknown")
}

/// Build a JavaScript guest from `guests/javascript` into a WIT component,
/// or `None` (test skipped) if its toolchain isn't installed.
pub fn js_component(name: &str) -> Option<PathBuf> {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let js = repo().join("guests/javascript");
    if !js.join("node_modules/componentize-qjs").exists() {
        let msg = "JavaScript guests need `npm ci` in guests/javascript";
        assert!(
            std::env::var_os("WASM_CORE_REQUIRE_ALL_GUESTS").is_none(),
            "{msg}"
        );
        eprintln!("{msg}; skipping");
        return None;
    }
    let dir = js.join(name);
    install_wit_deps(&dir);
    let out = repo()
        .join("target/guests/javascript")
        .join(format!("{name}.wasm"));
    let status = Command::new("node")
        .arg(js.join("build.mjs"))
        .arg(&dir)
        .arg(&out)
        .status()
        .expect("failed to run node");
    assert!(status.success(), "failed to build guest {name}");
    Some(out)
}

/// Records every emitted event and dispatches it on an in-process bus.
#[derive(Clone, Default)]
pub struct TestPlatform(Arc<State>);

#[derive(Default)]
pub struct State {
    pub bus: EventBus,
    pub emitted: Mutex<Vec<(String, Value)>>,
}

impl std::ops::Deref for TestPlatform {
    type Target = State;
    fn deref(&self) -> &State {
        &self.0
    }
}

impl Platform for TestPlatform {
    type ListenerId = u64;

    fn emit(&self, event: &str, payload: Value) -> std::result::Result<(), String> {
        self.emitted
            .lock()
            .unwrap()
            .push((event.to_string(), payload.clone()));
        self.bus.emit(event, &payload);
        Ok(())
    }

    fn listen(&self, event: &str, handler: EventHandler) -> u64 {
        self.bus.listen(event, handler)
    }

    fn unlisten(&self, id: u64) {
        self.bus.unlisten(id)
    }

    fn base_dir(&self, _: BaseDir) -> Result<PathBuf> {
        Ok(std::env::temp_dir())
    }
}

/// Minimal executor: the core's futures don't need a specific runtime.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

/// A builder on a fresh [`TestPlatform`], without hot reload.
pub fn test_builder() -> (TestPlatform, BackendBuilder<TestPlatform>) {
    let platform = TestPlatform::default();
    let builder = Backend::builder(platform.clone())
        .hot_reload(false)
        .typescript_options(TypeScriptOptions {
            generator: "test".into(),
            api_module: "my-api".into(),
        });
    (platform, builder)
}

/// A backend with an `echo` host command.
pub fn backend() -> (TestPlatform, Backend<TestPlatform>) {
    let (platform, builder) = test_builder();
    let backend = builder
        .host_command("echo", |call, args: Value| {
            Ok::<_, String>(json!({ "module": call.module, "args": args }))
        })
        .build()
        .unwrap();
    (platform, backend)
}

pub fn caps() -> Capabilities {
    Capabilities::default()
        .allow_emit("test://*")
        .allow_host_command("echo")
        .listen("app://ping")
}

/// Poll `command` until it returns something other than `null`.
pub fn wait_for_event(backend: &Backend<TestPlatform>, module: &str, command: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let last = backend.invoke(module, command, Value::Null).unwrap();
        if !last.is_null() {
            return last;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "event was not delivered"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
