//! The module registry: loads modules, routes invokes and events, and
//! implements the host side of the module API.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt::Display,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, Weak},
    time::SystemTime,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    Error, Result,
    capabilities::{Capabilities, Limits, Preopen, name_matches},
    config::{EngineKind, ExternalConfig, ModuleConfig},
    engine::{self, CommandInfo, ModuleHost, ModuleInstance, StateChanges},
    platform::{BaseDir, Platform, resolve_dir},
    state::StateStore,
};

/// Emitted with a [`ModuleInfo`] payload when a module is loaded.
pub const EVENT_MODULE_LOADED: &str = "wasm-backend://module-loaded";
/// Emitted with a [`ModuleInfo`] payload when a module is reloaded after its
/// file changed (or [`Backend::reload`] was called).
pub const EVENT_MODULE_RELOADED: &str = "wasm-backend://module-reloaded";
/// Emitted with the module name when a module is unloaded.
pub const EVENT_MODULE_UNLOADED: &str = "wasm-backend://module-unloaded";
/// Emitted with a [`ModuleError`] payload when loading or reloading fails.
pub const EVENT_MODULE_ERROR: &str = "wasm-backend://module-error";
/// Emitted with a [`ModuleTrap`] payload when a call into a module traps (a
/// panic, a timeout, running out of memory).
pub const EVENT_MODULE_TRAPPED: &str = "wasm-backend://module-trapped";

/// Decides whether a module may be loaded. Set with
/// [`BackendBuilder::load_gate`].
pub type LoadGate<P> =
    Arc<dyn Fn(&P, &LoadRequest<'_>) -> std::result::Result<(), String> + Send + Sync>;

/// A module about to be loaded or reloaded, as shown to the
/// [`BackendBuilder::load_gate`] before any of its code is compiled or run.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct LoadRequest<'a> {
    /// Name the module is loaded under.
    pub name: &'a str,
    /// File it is loaded from (`None` for embedded bytes).
    pub path: Option<&'a Path>,
    /// Whether it comes from the external plugins directory.
    pub external: bool,
    /// SHA-256 of the module's bytes, in lowercase hex. Compare it with a
    /// list of known plugins, or show it to the user.
    pub sha256: &'a str,
    /// What the module would be allowed to do.
    pub capabilities: &'a Capabilities,
    /// Whether this replaces a loaded module after its file changed.
    pub reload: bool,
}

/// A host command working on JSON values. Build one from a typed closure
/// with [`host_command_fn`].
pub type HostCommandFn<P> =
    Arc<dyn Fn(HostCall<'_, P>, Value) -> std::result::Result<Value, String> + Send + Sync>;

/// Context passed to host commands.
pub struct HostCall<'a, P> {
    /// The platform the backend runs on (e.g. it holds the app handle).
    pub platform: &'a P,
    /// Name of the module that invoked the command.
    pub module: &'a str,
}

/// Wrap a typed host command: arguments are deserialized from JSON, like
/// Tauri command arguments, and the result is serialized to JSON.
pub fn host_command_fn<P, A, T, E, F>(name: impl Into<String>, f: F) -> HostCommandFn<P>
where
    P: 'static,
    A: DeserializeOwned,
    T: Serialize,
    E: Display,
    F: Fn(HostCall<'_, P>, A) -> std::result::Result<T, E> + Send + Sync + 'static,
{
    let command = name.into();
    Arc::new(move |call, args| {
        let args = deserialize_args(args)
            .map_err(|e| format!("invalid arguments for host command `{command}`: {e}"))?;
        let out = f(call, args).map_err(|e| e.to_string())?;
        serde_json::to_value(out).map_err(|e| e.to_string())
    })
}

/// "No arguments" arrives as `null` (`invoke('cmd', 'null')`) or `{}` (a
/// typed import without parameters). Accept either for `()`, `Option`s and
/// structs whose fields all have defaults.
fn deserialize_args<A: DeserializeOwned>(args: Value) -> serde_json::Result<A> {
    let other = match &args {
        Value::Null => Some(Value::Object(Default::default())),
        Value::Object(o) if o.is_empty() => Some(Value::Null),
        _ => None,
    };
    serde_json::from_value(args).or_else(|e| match other {
        Some(other) => serde_json::from_value(other).map_err(|_| e),
        None => Err(e),
    })
}

/// Public description of a loaded module.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct ModuleInfo {
    pub name: String,
    /// The engine running the module (never `auto`).
    pub engine: EngineKind,
    /// File the module was loaded from, if any.
    pub path: Option<PathBuf>,
    /// Whether the module was loaded from the external plugins directory.
    pub external: bool,
    /// Whether the frontend may invoke the module without an explicit scope.
    /// Enforced by the integration (the Tauri plugin's `invoke` command);
    /// [`Backend::invoke`] itself is trusted and doesn't check it.
    pub exposed: bool,
    /// Whether the module is reloaded when its file changes.
    pub watched: bool,
    /// Incremented on every reload.
    pub generation: u64,
    /// SHA-256 of the module's bytes, in lowercase hex.
    #[serde(default)]
    pub sha256: String,
    /// Every interface the module imports, as it names them (e.g.
    /// `wasi:filesystem/types@0.2.6`; WIT modules only): all it can reach
    /// outside its own memory. A module importing nothing can only compute.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Calls that trapped since the module was last (re)loaded. After each
    /// one the module started again from fresh memory.
    #[serde(default)]
    pub traps: u32,
    /// Whether the module trapped more often than its `maxRestarts`
    /// capability allows. Every call fails until it is reloaded.
    #[serde(default)]
    pub stopped: bool,
    pub commands: Vec<CommandInfo>,
    /// Host commands the module imports as typed WIT functions, named as
    /// registered. Each one was checked when the module loaded: it exists
    /// and the module's capabilities allow it.
    #[serde(default)]
    pub imports: Vec<CommandInfo>,
    pub capabilities: Capabilities,
    /// Likely configuration mistakes found when the module was (re)loaded,
    /// e.g. a `hostCommands` entry that matches no registered host command.
    /// Also logged as warnings.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Payload of [`EVENT_MODULE_ERROR`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct ModuleError {
    pub module: String,
    pub error: String,
}

/// Payload of [`EVENT_MODULE_TRAPPED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct ModuleTrap {
    pub module: String,
    pub error: String,
    /// Traps since the module was last (re)loaded, this one included.
    pub traps: u32,
    /// Whether this trap stopped the module (see [`ModuleInfo::stopped`]).
    pub stopped: bool,
}

/// Why a module is known to the backend but not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum InactiveReason {
    /// Switched off: `enabled: false` in its configuration, or
    /// [`Backend::disable`]. [`Backend::enable`] loads it.
    Disabled,
    /// Refused: by the load gate, by the external plugins' `allow` and
    /// `deny` lists, or because its bytes don't match its `sha256`.
    Denied,
    /// It could not be loaded: a missing or invalid file, an import nothing
    /// provides, ...
    Failed,
}

/// A module the backend knows but isn't running, and why: the other half
/// of [`Backend::modules`]. See [`Backend::inactive`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct InactiveModule {
    pub name: String,
    /// File it would be loaded from, if any.
    pub path: Option<PathBuf>,
    /// Whether it comes from the external plugins directory.
    pub external: bool,
    /// SHA-256 of its bytes in lowercase hex, when they were read.
    pub sha256: Option<String>,
    pub reason: InactiveReason,
    /// The reason in words: the load gate's answer, the load error, ...
    pub message: String,
    /// What it would be allowed to do if it ran.
    pub capabilities: Capabilities,
}

/// An inactive module, with what is needed to load it after all.
pub(crate) struct Parked {
    config: ModuleConfig,
    /// File whose change retries the load (watched modules that failed).
    pub(crate) retry: Option<PathBuf>,
    info: InactiveModule,
}

/// How far loading a module got.
#[derive(Default)]
struct Source {
    path: Option<PathBuf>,
    sha256: Option<String>,
}

/// Metadata, kept apart from the instance so that reading it never waits
/// for (or deadlocks with) a running call.
struct Meta {
    info: ModuleInfo,
    typescript: String,
}

pub(crate) struct Module<P: Platform> {
    name: String,
    config: ModuleConfig,
    path: Option<PathBuf>,
    external: bool,
    watched: bool,
    instance: Mutex<Box<dyn ModuleInstance>>,
    meta: RwLock<Meta>,
    listeners: Mutex<Vec<P::ListenerId>>,
}

#[cfg_attr(not(feature = "hot-reload"), allow(dead_code))]
impl<P: Platform> Module<P> {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn watched(&self) -> bool {
        self.watched
    }

    fn external(&self) -> bool {
        self.external
    }

    fn meta(&self) -> std::sync::RwLockReadGuard<'_, Meta> {
        self.meta.read().unwrap_or_else(|e| e.into_inner())
    }

    fn instance(&self) -> std::sync::MutexGuard<'_, Box<dyn ModuleInstance>> {
        self.instance.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Resolved external plugins configuration.
pub(crate) struct External {
    pub(crate) dir: PathBuf,
    pub(crate) config: ExternalConfig,
}

/// Module state: every module's store in memory, read from the
/// [`StateStore`] when first needed and written back after each change.
struct States {
    loaded: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    store: Option<Arc<dyn StateStore>>,
}

impl States {
    /// Work on `module`'s state, reading it from the store first if needed.
    fn with<T>(
        &self,
        module: &str,
        f: impl FnOnce(&mut BTreeMap<String, String>) -> T,
    ) -> std::result::Result<T, String> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if !loaded.contains_key(module) {
            let state = match &self.store {
                Some(store) => store.load(module)?,
                None => BTreeMap::new(),
            };
            loaded.insert(module.to_string(), state);
        }
        Ok(f(loaded.entry(module.to_string()).or_default()))
    }

    /// Apply the writes of a call that returned, and store the result.
    fn commit(&self, module: &str, changes: StateChanges) -> std::result::Result<(), String> {
        if changes.is_empty() {
            return Ok(());
        }
        let state = self.with(module, |state| {
            for (key, value) in changes {
                match value {
                    Some(value) => state.insert(key, value),
                    None => state.remove(&key),
                };
            }
            state.clone()
        })?;
        match &self.store {
            Some(store) => store.save(module, &state),
            None => Ok(()),
        }
    }

    /// Remove everything stored for `module`.
    fn clear(&self, module: &str) -> std::result::Result<(), String> {
        self.loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(module.to_string(), BTreeMap::new());
        match &self.store {
            Some(store) => store.save(module, &BTreeMap::new()),
            None => Ok(()),
        }
    }
}

pub(crate) struct Inner<P: Platform> {
    pub(crate) platform: P,
    modules: RwLock<BTreeMap<String, Arc<Module<P>>>>,
    /// Sorted, so name lookups and listings are deterministic.
    host_commands: BTreeMap<String, HostCommandFn<P>>,
    pub(crate) hot_reload: bool,
    dev: bool,
    pub(crate) external: Option<External>,
    typescript_dir: Option<PathBuf>,
    typescript: TypeScriptOptions,
    load_gate: Option<LoadGate<P>>,
    limits: Limits,
    states: States,
    /// Modules that are known but not running.
    pub(crate) inactive: Mutex<BTreeMap<String, Parked>>,
    #[cfg(feature = "hot-reload")]
    pub(crate) watcher: Mutex<Option<crate::watcher::Watcher>>,
}

/// Header of generated TypeScript bindings.
#[derive(Debug, Clone)]
pub struct TypeScriptOptions {
    /// Named in the `// Generated by ...` comment.
    pub generator: String,
    /// Module the bindings import `invokeModule(module, command, args)` from.
    pub api_module: String,
}

impl Default for TypeScriptOptions {
    fn default() -> Self {
        Self {
            generator: "wasm-backend-core".into(),
            api_module: "wasm-backend-api".into(),
        }
    }
}

/// The module registry: loads modules, routes invokes and events. Cheap to
/// clone (it's reference counted). Create one with [`Backend::builder`].
pub struct Backend<P: Platform>(pub(crate) Arc<Inner<P>>);

impl<P: Platform> Clone for Backend<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

thread_local! {
    /// Modules currently executing on this thread, to detect reentrancy.
    static CALL_STACK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

struct CallGuard;

impl CallGuard {
    fn enter(module: &str) -> Result<Self> {
        CALL_STACK.with(|s| {
            let mut s = s.borrow_mut();
            if s.iter().any(|m| m == module) {
                return Err(Error::Reentrant(module.to_string()));
            }
            s.push(module.to_string());
            Ok(CallGuard)
        })
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        CALL_STACK.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// Bridges module host calls to the app. Holds a weak reference so modules
/// don't keep the registry alive.
struct Bridge<P: Platform>(Weak<Inner<P>>);

impl<P: Platform> engine::Host for Bridge<P> {
    fn emit(&self, event: &str, payload: Value) -> std::result::Result<(), String> {
        let inner = self.0.upgrade().ok_or("app is shutting down")?;
        if !valid_event_name(event) {
            return Err(format!(
                "invalid event name `{event}`: only alphanumeric characters, `-`, `/`, `:` and `_` are allowed"
            ));
        }
        inner.platform.emit(event, payload)
    }

    fn invoke(
        &self,
        module: &str,
        command: &str,
        args: Value,
    ) -> std::result::Result<Value, String> {
        let inner = self.0.upgrade().ok_or("app is shutting down")?;
        let f = inner
            .host_commands
            .get(command)
            .ok_or_else(|| format!("unknown host command `{command}`"))?
            .clone();
        f(
            HostCall {
                platform: &inner.platform,
                module,
            },
            args,
        )
    }

    fn resolve_command(&self, name: &str) -> Option<String> {
        let inner = self.0.upgrade()?;
        resolve_host_command(&inner.host_commands, name).map(str::to_string)
    }

    fn command_names(&self) -> Vec<String> {
        self.0
            .upgrade()
            .map(|inner| inner.host_commands.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn state_get(&self, module: &str, key: &str) -> std::result::Result<Option<String>, String> {
        let inner = self.0.upgrade().ok_or("app is shutting down")?;
        inner.states.with(module, |state| state.get(key).cloned())
    }

    fn state_keys(&self, module: &str) -> std::result::Result<Vec<String>, String> {
        let inner = self.0.upgrade().ok_or("app is shutting down")?;
        inner
            .states
            .with(module, |state| state.keys().cloned().collect())
    }
}

/// Find a host command by name: exactly, else across naming conventions
/// (`app-version` and `appVersion` find `app_version`), like module commands.
fn resolve_host_command<'a, F>(commands: &'a BTreeMap<String, F>, name: &str) -> Option<&'a str> {
    if let Some((k, _)) = commands.get_key_value(name) {
        return Some(k);
    }
    let wanted = crate::naming::to_kebab(name);
    commands
        .keys()
        .find(|k| crate::naming::to_kebab(k) == wanted)
        .map(String::as_str)
}

pub(crate) fn valid_event_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '/' | ':' | '_'))
}

/// A single-quoted JavaScript string literal.
fn js_single_quoted(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' | '\'' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn quoted_list(items: &[impl AsRef<str>]) -> String {
    items
        .iter()
        .map(|i| format!("`{}`", i.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What is said about an external plugin its `allow` and `deny` lists keep
/// out.
fn refused_plugin(name: &str, reason: &str) -> String {
    format!("loading external plugin `{name}` was refused: {reason}")
}

/// The SHA-256 of `bytes`, in lowercase hex.
fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Configures a [`Backend`].
pub struct BackendBuilder<P: Platform> {
    platform: P,
    host_commands: BTreeMap<String, HostCommandFn<P>>,
    hot_reload: bool,
    dev: bool,
    external: Option<ExternalConfig>,
    typescript_dir: Option<PathBuf>,
    typescript: TypeScriptOptions,
    load_gate: Option<LoadGate<P>>,
    limits: Limits,
    state_store: Option<Arc<dyn StateStore>>,
}

impl<P: Platform> BackendBuilder<P> {
    /// Register a host command that modules can call, if their
    /// capabilities allow it: through the host `invoke` function, or as a
    /// typed function a WIT module imports (checked when it loads). Names
    /// match across `snake_case`, `kebab-case` and `camelCase`. See
    /// [`host_command_fn`].
    pub fn host_command<A, T, E, F>(self, name: impl Into<String>, f: F) -> Self
    where
        A: DeserializeOwned,
        T: Serialize,
        E: Display,
        F: Fn(HostCall<'_, P>, A) -> std::result::Result<T, E> + Send + Sync + 'static,
    {
        let name = name.into();
        let f = host_command_fn(name.clone(), f);
        self.host_command_fn(name, f)
    }

    /// Register an untyped host command.
    pub fn host_command_fn(mut self, name: impl Into<String>, f: HostCommandFn<P>) -> Self {
        self.host_commands.insert(name.into(), f);
        self
    }

    /// Watch module files and reload them on change (default: enabled in
    /// debug builds). Needs the `hot-reload` feature.
    pub fn hot_reload(mut self, enabled: bool) -> Self {
        self.hot_reload = enabled;
        self
    }

    /// Development mode (default: debug builds): modules load from
    /// `dev_path` when set, and relative module paths resolve against the
    /// current directory instead of [`BaseDir::Resource`].
    pub fn dev(mut self, enabled: bool) -> Self {
        self.dev = enabled;
        self
    }

    /// Load third-party plugins from a directory.
    pub fn external(mut self, config: Option<ExternalConfig>) -> Self {
        self.external = config;
        self
    }

    /// Write TypeScript bindings for every module to `<dir>/<module>.ts`
    /// whenever it is loaded or reloaded.
    pub fn typescript_out_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.typescript_dir = dir;
        self
    }

    /// Header of the generated TypeScript bindings.
    pub fn typescript_options(mut self, options: TypeScriptOptions) -> Self {
        self.typescript = options;
        self
    }

    /// Ask `gate` before any module is loaded or reloaded. It sees what is
    /// about to run (see [`LoadRequest`]) before a byte of it is compiled;
    /// returning `Err(reason)` refuses the load, which fails with
    /// [`Error::NotAllowed`] (and a refused reload keeps the previous
    /// version running).
    ///
    /// Use it to decide which third-party plugins run: check
    /// [`LoadRequest::sha256`] against a list the user approved, say. The
    /// same file may be asked about again (it is retried when the plugins
    /// directory changes), so remember the answer rather than asking the
    /// user twice.
    pub fn load_gate<F>(mut self, gate: F) -> Self
    where
        F: Fn(&P, &LoadRequest<'_>) -> std::result::Result<(), String> + Send + Sync + 'static,
    {
        self.load_gate = Some(Arc::new(gate));
        self
    }

    /// Resource limits (memory, time per call, restarts) for every module
    /// that doesn't set its own in its [`Capabilities`]: app modules,
    /// external plugins and modules loaded at runtime alike.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Keep module state (what modules store through `tauri:backend/state`)
    /// in `store`, so it outlives the app: [`FileStateStore`](crate::FileStateStore)
    /// for one JSON file per module, or your own [`StateStore`]. Without
    /// one, state survives reloads and traps but not the backend.
    pub fn state_store(mut self, store: impl StateStore) -> Self {
        self.state_store = Some(Arc::new(store));
        self
    }

    /// Create the backend. No module is loaded yet: call
    /// [`Backend::load_all`] (or [`Backend::load`]) next. Splitting the two
    /// lets an integration publish the backend (e.g. as app state) before
    /// modules start running.
    pub fn build(self) -> Result<Backend<P>> {
        let mut seen = BTreeMap::new();
        for name in self.host_commands.keys() {
            if let Some(other) = seen.insert(crate::naming::to_kebab(name), name) {
                log::warn!(
                    "host commands `{other}` and `{name}` only differ in naming convention; other spellings of them resolve to `{other}`"
                );
            }
        }
        let external = match self.external {
            Some(config) => {
                let dir = resolve_dir(&self.platform, &config.dir, BaseDir::AppData)?;
                // Create it before the watcher starts so it can be watched.
                std::fs::create_dir_all(&dir)?;
                Some(External { dir, config })
            }
            None => None,
        };
        Ok(Backend(Arc::new(Inner {
            platform: self.platform,
            modules: RwLock::new(BTreeMap::new()),
            host_commands: self.host_commands,
            hot_reload: self.hot_reload,
            dev: self.dev,
            external,
            typescript_dir: self.typescript_dir,
            typescript: self.typescript,
            load_gate: self.load_gate,
            limits: self.limits,
            states: States {
                loaded: Mutex::new(BTreeMap::new()),
                store: self.state_store,
            },
            inactive: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "hot-reload")]
            watcher: Mutex::new(None),
        })))
    }
}

impl<P: Platform> Backend<P> {
    pub fn builder(platform: P) -> BackendBuilder<P> {
        BackendBuilder {
            platform,
            host_commands: BTreeMap::new(),
            hot_reload: cfg!(debug_assertions),
            dev: cfg!(debug_assertions),
            external: None,
            typescript_dir: None,
            typescript: TypeScriptOptions::default(),
            load_gate: None,
            limits: Limits::default(),
            state_store: None,
        }
    }

    /// The platform the backend runs on.
    pub fn platform(&self) -> &P {
        &self.0.platform
    }

    /// Load the configured modules (reporting failures through
    /// [`EVENT_MODULE_ERROR`] instead of failing, so a broken module can be
    /// fixed and hot reloaded), scan the external plugins directory and
    /// start watching files.
    pub fn load_all(&self, modules: impl IntoIterator<Item = (String, ModuleConfig)>) {
        for (name, module) in modules {
            match self.load(&name, module) {
                Ok(_) => {}
                Err(Error::Disabled(_)) => log::info!("WASM module `{name}` is disabled"),
                Err(e) => self.report_error(&name, &e),
            }
        }
        self.sync_external();
        #[cfg(feature = "hot-reload")]
        crate::watcher::sync(self);
    }

    /// Invoke a module command, like the frontend's `invoke`. `args` is an
    /// object keyed by parameter name for WIT modules; Extism modules receive
    /// it as their JSON input.
    ///
    /// This runs WASM on the calling thread; from async code, prefer
    /// [`Backend::invoke_async`].
    pub fn invoke(&self, module: &str, command: &str, args: Value) -> Result<Value> {
        let m = self.get(module)?;
        self.run(&m, |instance| instance.call(command, args))
    }

    /// Run one call into a module, unless it was stopped, and count the
    /// trap if it ends in one.
    fn run<T>(
        &self,
        m: &Module<P>,
        call: impl FnOnce(&mut dyn ModuleInstance) -> Result<T>,
    ) -> Result<T> {
        let _guard = CallGuard::enter(&m.name)?;
        let mut instance = m.instance();
        {
            let meta = m.meta();
            if meta.info.stopped {
                return Err(Error::Stopped {
                    module: m.name.clone(),
                    traps: meta.info.traps,
                });
            }
        }
        let before = instance.traps();
        let result = call(instance.as_mut());
        let trapped = instance.traps() != before;
        // What the call wrote to the module's state is kept only if it
        // returned: a trap leaves the state as it was before the call.
        let written = instance.host().take_pending();
        if !trapped {
            self.commit_state(&m.name, written);
        }
        // Counted before the next call can start, so the one after the last
        // allowed restart never runs.
        let trap = match &result {
            Err(error) if trapped => {
                let mut meta = m.meta.write().unwrap_or_else(|e| e.into_inner());
                meta.info.traps = meta.info.traps.saturating_add(1);
                meta.info.stopped = m
                    .config
                    .capabilities
                    .max_restarts
                    .is_some_and(|max| meta.info.traps > max);
                Some(ModuleTrap {
                    module: m.name.clone(),
                    error: error.to_string(),
                    traps: meta.info.traps,
                    stopped: meta.info.stopped,
                })
            }
            _ => None,
        };
        drop(instance);
        if let Some(trap) = trap {
            if trap.stopped {
                log::error!(
                    "WASM module `{}` was stopped after trapping {} times; reload it to start it again",
                    trap.module,
                    trap.traps
                );
            }
            self.emit_lifecycle(EVENT_MODULE_TRAPPED, &trap);
        }
        result
    }

    /// [`Backend::invoke`] on a blocking thread, via
    /// [`Platform::spawn_blocking`]. The future works with any async runtime
    /// (tokio, smol, GPUI's executors, ...).
    pub async fn invoke_async(&self, module: &str, command: &str, args: Value) -> Result<Value> {
        let this = self.clone();
        let (module, command) = (module.to_string(), command.to_string());
        match crate::task::run_blocking(&self.0.platform, move || {
            this.invoke(&module, &command, args)
        })
        .await
        {
            Some(Ok(result)) => result,
            Some(Err(panic)) => Err(Error::Runtime(format!("the call panicked: {panic}"))),
            None => Err(Error::Runtime(
                "the platform dropped the call before it ran".into(),
            )),
        }
    }

    /// Typed variant of [`Backend::invoke`].
    pub fn call<A: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        module: &str,
        command: &str,
        args: A,
    ) -> Result<T> {
        let args = serde_json::to_value(args).map_err(|e| Error::InvalidArgs {
            command: command.to_string(),
            message: e.to_string(),
        })?;
        let out = self.invoke(module, command, args)?;
        serde_json::from_value(out).map_err(|e| Error::Runtime(format!("unexpected output: {e}")))
    }

    /// Deliver an event directly to a module's event handler, regardless of
    /// its `events.listen` subscriptions.
    pub fn send_event(&self, module: &str, event: &str, payload: &Value) -> Result<()> {
        let m = self.get(module)?;
        self.run(&m, |instance| instance.on_event(event, payload))
    }

    /// Store state writes, reporting a store that fails: the call that made
    /// them has returned, so there is nobody to hand the error to.
    fn commit_state(&self, module: &str, written: StateChanges) {
        if let Err(e) = self.0.states.commit(module, written) {
            self.report_error(module, &Error::State(e));
        }
    }

    /// A module's state store: what it wrote through `tauri:backend/state`
    /// (`tauri_state_*` for Extism), whether or not it is loaded right now.
    /// Writes of a call still running are not in it yet.
    pub fn state(&self, module: &str) -> Result<BTreeMap<String, String>> {
        self.0
            .states
            .with(module, |state| state.clone())
            .map_err(Error::State)
    }

    /// Remove everything a module stored, e.g. when the user uninstalls a
    /// plugin. Unloading a module keeps its state; a module loaded under
    /// the same name later finds it again.
    pub fn clear_state(&self, module: &str) -> Result<()> {
        self.0.states.clear(module).map_err(Error::State)
    }

    /// Information about every loaded module. [`Backend::inactive`] lists
    /// the ones that are not.
    pub fn modules(&self) -> Vec<ModuleInfo> {
        let modules: Vec<_> = self.read_modules().values().cloned().collect();
        modules.iter().map(|m| m.meta().info.clone()).collect()
    }

    /// Every module the backend knows but isn't running, and why: switched
    /// off, refused (by the load gate, the external plugins' `allow` and
    /// `deny` lists or its `sha256`) or failed to load. Together with
    /// [`Backend::modules`] this is everything that was configured, found
    /// in the plugins directory or loaded at runtime.
    ///
    /// A module leaves the list when it loads ([`Backend::enable`], a
    /// changed file, the load gate changing its mind), when
    /// [`Backend::unload`] forgets it, or when an external plugin's file is
    /// removed.
    pub fn inactive(&self) -> Vec<InactiveModule> {
        let parked: Vec<_> = self.parked().values().map(|p| p.info.clone()).collect();
        // A failed replacement of a module that still runs is not inactive.
        let modules = self.read_modules();
        parked
            .into_iter()
            .filter(|p| !modules.contains_key(&p.name))
            .collect()
    }

    /// Switch a module off: it is unloaded and stays that way, an external
    /// plugin too, until [`Backend::enable`] is called. Its state is kept.
    ///
    /// This lasts as long as the backend. To keep a module off between runs
    /// of the app, remember the user's choice and set `enabled: false` in
    /// its configuration (or call this again at startup).
    pub fn disable(&self, name: &str) -> Result<()> {
        const MESSAGE: &str = "switched off with `disable`";
        match self.get(name) {
            Ok(m) => {
                let mut config = m.config.clone();
                config.enabled = Some(false);
                let info = InactiveModule {
                    name: name.to_string(),
                    path: m.path.clone(),
                    external: m.external,
                    sha256: Some(m.meta().info.sha256.clone()),
                    reason: InactiveReason::Disabled,
                    message: MESSAGE.to_string(),
                    capabilities: config.capabilities.clone(),
                };
                // Parked first, so a scan of the plugins directory running
                // now doesn't load it again.
                self.parked().insert(
                    name.to_string(),
                    Parked {
                        config,
                        retry: None,
                        info,
                    },
                );
                self.remove(name)
            }
            Err(e) => {
                {
                    let mut parked = self.parked();
                    let Some(p) = parked.get_mut(name) else {
                        return Err(e);
                    };
                    p.config.enabled = Some(false);
                    p.retry = None;
                    p.info.reason = InactiveReason::Disabled;
                    p.info.message = MESSAGE.to_string();
                }
                #[cfg(feature = "hot-reload")]
                crate::watcher::sync(self);
                Ok(())
            }
        }
    }

    /// Load a module that is in [`Backend::inactive`]: one that was switched
    /// off, or one that was refused or failed and should be tried again
    /// (e.g. after the user approved it). The load gate is asked as for any
    /// load. Does nothing for a module that is already loaded.
    pub fn enable(&self, name: &str) -> Result<ModuleInfo> {
        let parked = self
            .parked()
            .get(name)
            .map(|p| (p.config.clone(), p.info.external));
        let Some((mut config, external)) = parked else {
            return self.module(name);
        };
        config.enabled = Some(true);
        let result = self.load_inner(name, config, external);
        if let Err(e) = &result {
            self.report_error(name, e);
        }
        result
    }

    pub(crate) fn parked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Parked>> {
        self.0.inactive.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Information about one module.
    pub fn module(&self, name: &str) -> Result<ModuleInfo> {
        let m = self.get(name)?;
        let info = m.meta().info.clone();
        Ok(info)
    }

    /// TypeScript bindings for a module's commands, built from the types
    /// the module exports. See the README for how to use them.
    pub fn typescript(&self, name: &str) -> Result<String> {
        let m = self.get(name)?;
        let body = m.meta().typescript.clone();
        let TypeScriptOptions {
            generator,
            api_module,
        } = &self.0.typescript;
        Ok(format!(
            "// Generated by {generator} from module `{name}`. Do not edit.\n\
             import {{ invokeModule }} from {api}\n\n\
             export const {ident} = {{\n{body}}}\n\nexport default {ident}\n",
            api = js_single_quoted(api_module),
            ident = crate::naming::to_camel(name),
        ))
    }

    /// Load (or replace) a module at runtime. Use this to manage plugins
    /// yourself, e.g. after the user picked a file and granted permissions.
    pub fn load(&self, name: &str, config: ModuleConfig) -> Result<ModuleInfo> {
        self.load_inner(name, config, false)
    }

    /// Unload a module and forget it: it is not listed as inactive either.
    /// An external plugin whose file is still there is loaded again by the
    /// next scan of the plugins directory; use [`Backend::disable`] to keep
    /// one off.
    pub fn unload(&self, name: &str) -> Result<()> {
        let parked = self.parked().remove(name).is_some();
        match self.remove(name) {
            Err(_) if parked => {
                #[cfg(feature = "hot-reload")]
                crate::watcher::sync(self);
                Ok(())
            }
            result => result,
        }
    }

    /// Take a loaded module out of the registry.
    fn remove(&self, name: &str) -> Result<()> {
        let m = self
            .0
            .modules
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
            .ok_or_else(|| Error::ModuleNotFound(name.to_string()))?;
        for id in m
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            self.0.platform.unlisten(id);
        }
        #[cfg(feature = "hot-reload")]
        crate::watcher::sync(self);
        let _ = self
            .0
            .platform
            .emit(EVENT_MODULE_UNLOADED, Value::from(name));
        Ok(())
    }

    /// Reload a module from its file, even if the file is unchanged (which
    /// resets the module's state, and starts a stopped module again). Blocks
    /// while the module compiles.
    pub fn reload(&self, name: &str) -> Result<ModuleInfo> {
        let m = self.get(name)?;
        let result = self
            .reload_module(&m, false)
            .map(|info| info.expect("forced reloads always reload"));
        if let Err(e) = &result {
            self.report_error(name, e);
        }
        result
    }

    pub(crate) fn reload_module(
        &self,
        m: &Module<P>,
        only_if_changed: bool,
    ) -> Result<Option<ModuleInfo>> {
        let Some(path) = &m.path else {
            return Err(Error::NotAllowed(format!(
                "module `{}` is embedded and can't be reloaded",
                m.name
            )));
        };
        let bytes = std::fs::read(path)?;
        let new_hash = sha256(&bytes);
        if only_if_changed && m.meta().info.sha256 == new_hash {
            return Ok(None);
        }
        // Compile and instantiate outside the lock so calls keep being served
        // by the old instance meanwhile.
        let started = SystemTime::now();
        let (instance, kind) = self.instantiate(
            &LoadRequest {
                name: &m.name,
                path: Some(path),
                external: m.external,
                sha256: &new_hash,
                capabilities: &m.config.capabilities,
                reload: true,
            },
            &m.config,
            &bytes,
        )?;
        let _guard = CallGuard::enter(&m.name)?;
        // The module may have been unloaded or replaced while compiling.
        let current = self.get(&m.name)?;
        if !std::ptr::eq(Arc::as_ptr(&current), m) {
            return Err(Error::ModuleNotFound(m.name.clone()));
        }
        let commands = instance.commands();
        let imports = instance.imports();
        let requires = instance.requires();
        let warnings = self.lint(&m.name, &m.config.capabilities, instance.as_ref());
        let typescript = instance.typescript(&m.name).unwrap_or_default();
        // Anything it stored while starting up.
        self.commit_state(&m.name, instance.host().take_pending());
        *m.instance() = instance;
        let info = {
            let mut meta = m.meta.write().unwrap_or_else(|e| e.into_inner());
            meta.info.commands = commands;
            meta.info.imports = imports;
            meta.info.requires = requires;
            meta.info.traps = 0;
            meta.info.stopped = false;
            meta.info.warnings = warnings;
            meta.info.engine = kind;
            meta.info.generation += 1;
            meta.typescript = typescript;
            meta.info.sha256 = new_hash;
            meta.info.clone()
        };
        log::info!(
            "reloaded WASM module `{}` in {:?}",
            m.name,
            started.elapsed().unwrap_or_default()
        );
        self.write_typescript(&m.name);
        self.emit_lifecycle(EVENT_MODULE_RELOADED, &info);
        Ok(Some(info))
    }

    fn emit_lifecycle(&self, event: &str, payload: &impl Serialize) {
        if let Ok(payload) = serde_json::to_value(payload) {
            let _ = self.0.platform.emit(event, payload);
        }
    }

    /// Log an error and emit it as [`EVENT_MODULE_ERROR`].
    pub fn report_error(&self, module: &str, error: &Error) {
        log::error!("WASM module `{module}`: {error}");
        self.emit_lifecycle(
            EVENT_MODULE_ERROR,
            &ModuleError {
                module: module.to_string(),
                error: error.to_string(),
            },
        );
    }

    pub(crate) fn read_modules(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<Module<P>>>> {
        self.0.modules.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self, name: &str) -> Result<Arc<Module<P>>> {
        if let Some(m) = self.read_modules().get(name) {
            return Ok(m.clone());
        }
        let disabled = self
            .parked()
            .get(name)
            .is_some_and(|p| p.info.reason == InactiveReason::Disabled);
        Err(if disabled {
            Error::Disabled(name.to_string())
        } else {
            Error::ModuleNotFound(name.to_string())
        })
    }

    /// Whether the frontend may invoke the module without an explicit scope
    /// (`false` for unknown modules).
    pub fn is_exposed(&self, name: &str) -> bool {
        self.get(name)
            .map(|m| m.meta().info.exposed)
            .unwrap_or(false)
    }

    /// Configuration that is valid but almost certainly not what was meant.
    /// Not errors: e.g. the capabilities shared by all external plugins may
    /// subscribe to events some plugins don't handle.
    fn lint(&self, name: &str, caps: &Capabilities, instance: &dyn ModuleInstance) -> Vec<String> {
        let mut warnings = Vec::new();
        if !caps.events.listen.is_empty() && !instance.handles_events() {
            warnings.push(format!(
                "`capabilities.events.listen` subscribes to {} but the module has no event handler, so these events are dropped (WIT: include `tauri:backend/module-with-events`; Extism: export `on_event`)",
                quoted_list(&caps.events.listen)
            ));
        }
        if caps.state.is_empty()
            && instance
                .requires()
                .iter()
                .any(|i| i.starts_with("tauri:backend/state"))
        {
            warnings.push(
                "the module imports `tauri:backend/state` but `capabilities.state` grants no keys, so every state call is refused (add e.g. `\"state\": [\"*\"]`)"
                    .to_string(),
            );
        }
        let registered: Vec<&String> = self.0.host_commands.keys().collect();
        // `!` entries deny; one that matches nothing takes nothing away.
        for pattern in caps.host_commands.iter().filter(|p| !p.starts_with('!')) {
            if !registered.iter().any(|c| name_matches(pattern, c)) {
                warnings.push(if registered.is_empty() {
                    format!("`capabilities.hostCommands` allows `{pattern}`, but the app registers no host commands")
                } else {
                    format!(
                        "`capabilities.hostCommands` entry `{pattern}` matches no registered host command (registered: {})",
                        quoted_list(&registered)
                    )
                });
            }
        }
        for w in &warnings {
            log::warn!("WASM module `{name}`: {w}");
        }
        warnings
    }

    /// Ask the load gate, then compile and instantiate the module.
    fn instantiate(
        &self,
        request: &LoadRequest<'_>,
        config: &ModuleConfig,
        bytes: &[u8],
    ) -> Result<(Box<dyn ModuleInstance>, EngineKind)> {
        let name = request.name;
        // Here rather than where the plugins directory is scanned, so that
        // no way of loading a plugin (`enable`, a reload) gets around it.
        if request.external
            && let Some(external) = &self.0.external
            && let Some(reason) = external.config.refusal(name)
        {
            return Err(Error::NotAllowed(refused_plugin(name, &reason)));
        }
        if let Some(pinned) = &config.sha256
            && !pinned.trim().eq_ignore_ascii_case(request.sha256)
        {
            return Err(Error::NotAllowed(format!(
                "loading WASM module `{name}` was refused: the SHA-256 of its bytes is {}, not the configured `sha256` ({})",
                request.sha256,
                pinned.trim()
            )));
        }
        if let Some(gate) = &self.0.load_gate {
            gate(&self.0.platform, request).map_err(|reason| {
                Error::NotAllowed(format!(
                    "loading WASM module `{name}` (sha256 {}) was refused: {reason}",
                    request.sha256
                ))
            })?;
        }
        let kind = engine::detect(bytes, config.engine);
        let host = Arc::new(ModuleHost {
            module: name.to_string(),
            capabilities: self.resolve_capabilities(&config.capabilities)?,
            host: Arc::new(Bridge(Arc::downgrade(&self.0))),
            pending: Mutex::default(),
        });
        Ok((engine::instantiate(kind, bytes, host)?, kind))
    }

    pub(crate) fn load_inner(
        &self,
        name: &str,
        mut config: ModuleConfig,
        external: bool,
    ) -> Result<ModuleInfo> {
        config.capabilities.apply_limits(&self.0.limits);
        let mut source = Source::default();
        let result = self.try_load(name, &config, external, &mut source);
        match &result {
            Ok(_) => {
                self.parked().remove(name);
            }
            Err(error) => {
                let reason = match error {
                    Error::Disabled(_) => InactiveReason::Disabled,
                    Error::NotAllowed(_) => InactiveReason::Denied,
                    _ => InactiveReason::Failed,
                };
                let message = match reason {
                    InactiveReason::Disabled => "`enabled` is `false`".to_string(),
                    _ => error.to_string(),
                };
                // Retry when the file shows up or changes (e.g. after a build).
                let watch = config.watch.unwrap_or(self.0.hot_reload);
                let retry = (watch && reason != InactiveReason::Disabled)
                    .then(|| source.path.clone())
                    .flatten();
                let info = InactiveModule {
                    name: name.to_string(),
                    path: source.path,
                    external,
                    sha256: source.sha256,
                    reason,
                    message,
                    capabilities: config.capabilities.clone(),
                };
                self.parked().insert(
                    name.to_string(),
                    Parked {
                        config,
                        retry,
                        info,
                    },
                );
            }
        }
        #[cfg(feature = "hot-reload")]
        if result.is_err() {
            crate::watcher::sync(self);
        }
        result
    }

    /// Retry loading a module that previously failed.
    #[cfg_attr(not(feature = "hot-reload"), allow(dead_code))]
    pub(crate) fn retry_pending(&self, name: &str) {
        let entry = self
            .parked()
            .get(name)
            .filter(|p| p.retry.is_some())
            .map(|p| (p.config.clone(), p.info.external));
        if let Some((config, external)) = entry
            && let Err(e) = self.load_inner(name, config, external)
        {
            self.report_error(name, &e);
        }
    }

    fn write_typescript(&self, name: &str) {
        let Some(dir) = &self.0.typescript_dir else {
            return;
        };
        let result = self.typescript(name).and_then(|ts| {
            std::fs::create_dir_all(dir)?;
            let path = dir.join(format!("{name}.ts"));
            // Avoid touching the file (and triggering frontend HMR) needlessly.
            if std::fs::read_to_string(&path).ok().as_deref() != Some(ts.as_str()) {
                std::fs::write(&path, ts)?;
            }
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("failed to write TypeScript bindings for `{name}`: {e}");
        }
    }

    fn try_load(
        &self,
        name: &str,
        config: &ModuleConfig,
        external: bool,
        source: &mut Source,
    ) -> Result<ModuleInfo> {
        let load_err = |message: String| Error::Load {
            module: name.to_string(),
            message,
        };
        for event in &config.capabilities.events.listen {
            if !valid_event_name(event) {
                return Err(load_err(format!(
                    "invalid event name `{event}` in `events.listen`"
                )));
            }
        }
        let path = match (&config.bytes, config.configured_path(self.0.dev)) {
            (Some(_), _) => None,
            (None, Some(path)) => Some(self.resolve_module_path(path, external)?),
            (None, None) => return Err(load_err("no `path` configured".into())),
        };
        source.path = path.clone();
        if config.enabled == Some(false) {
            return Err(Error::Disabled(name.to_string()));
        }
        let bytes: Arc<[u8]> = match (&config.bytes, &path) {
            (Some(bytes), _) => bytes.clone(),
            (None, Some(path)) => std::fs::read(path)
                .map_err(|e| load_err(format!("can't read {}: {e}", path.display())))?
                .into(),
            (None, None) => unreachable!("a module without bytes has a path"),
        };

        let started = SystemTime::now();
        let hash = sha256(&bytes);
        source.sha256 = Some(hash.clone());
        let (instance, kind) = self.instantiate(
            &LoadRequest {
                name,
                path: path.as_deref(),
                external,
                sha256: &hash,
                capabilities: &config.capabilities,
                reload: false,
            },
            config,
            &bytes,
        )?;
        let watched = path.is_some() && config.watch.unwrap_or(self.0.hot_reload);
        // Anything it stored while starting up.
        self.commit_state(name, instance.host().take_pending());
        let warnings = self.lint(name, &config.capabilities, instance.as_ref());
        let info = ModuleInfo {
            name: name.to_string(),
            engine: kind,
            path: path.clone(),
            external,
            exposed: config.expose.unwrap_or(!external),
            watched,
            generation: 0,
            sha256: hash,
            requires: instance.requires(),
            traps: 0,
            stopped: false,
            commands: instance.commands(),
            imports: instance.imports(),
            capabilities: config.capabilities.clone(),
            warnings,
        };
        let module = Arc::new(Module {
            name: name.to_string(),
            external,
            watched,
            meta: RwLock::new(Meta {
                info: info.clone(),
                typescript: instance.typescript(name).unwrap_or_default(),
            }),
            instance: Mutex::new(instance),
            listeners: Mutex::new(Vec::new()),
            config: config.clone(),
            path,
        });
        self.subscribe(&module);

        let previous = self
            .0
            .modules
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), module);
        if let Some(previous) = previous {
            for id in previous
                .listeners
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
            {
                self.0.platform.unlisten(id);
            }
        }
        #[cfg(feature = "hot-reload")]
        crate::watcher::sync(self);

        log::info!(
            "loaded WASM module `{name}` ({kind:?}, {} commands) in {:?}",
            info.commands.len(),
            started.elapsed().unwrap_or_default()
        );
        self.write_typescript(name);
        self.emit_lifecycle(EVENT_MODULE_LOADED, &info);
        Ok(info)
    }

    /// Load new plugins from the external plugins directory and unload the
    /// ones whose file was removed. Plugins that were refused or failed
    /// before are tried again; disabled ones are left alone.
    pub fn sync_external(&self) {
        let Some(ext) = &self.0.external else { return };
        if let Err(e) = std::fs::create_dir_all(&ext.dir) {
            log::error!("can't create plugins directory {}: {e}", ext.dir.display());
            return;
        }
        let mut present = Vec::new();
        match std::fs::read_dir(&ext.dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("wasm") || !path.is_file()
                    {
                        continue;
                    }
                    let Some(name) = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    present.push(name.clone());
                    // What the name stands for already: `(external, settled)`,
                    // settled being loaded or switched off.
                    let known = match self.read_modules().get(&name) {
                        Some(m) => Some((m.external(), true)),
                        None => self
                            .parked()
                            .get(&name)
                            .map(|p| (p.info.external, p.info.reason == InactiveReason::Disabled)),
                    };
                    match known {
                        Some((false, _)) => log::warn!(
                            "ignoring external plugin {}: an app module named `{name}` exists",
                            path.display()
                        ),
                        Some((true, true)) => {}
                        // New, or refused or failed before: try (again).
                        _ => match ext.config.refusal(&name) {
                            Some(reason) => self.park_refused(&name, &path, &ext.config, reason),
                            None => {
                                let config = ext.config.module(&name, &path);
                                match self.load_inner(&name, config, true) {
                                    Ok(_) | Err(Error::Disabled(_)) => {}
                                    Err(e) => self.report_error(&name, &e),
                                }
                            }
                        },
                    }
                }
            }
            Err(e) => log::error!("can't read plugins directory {}: {e}", ext.dir.display()),
        }
        let gone: Vec<String> = self
            .read_modules()
            .values()
            .filter(|m| m.external() && !present.contains(&m.name))
            .map(|m| m.name.clone())
            .collect();
        for name in gone {
            let _ = self.unload(&name);
        }
        self.parked()
            .retain(|name, p| !p.info.external || present.contains(name));
    }

    /// List an external plugin the `allow` and `deny` lists keep out, without
    /// reading more of it than its hash.
    fn park_refused(&self, name: &str, path: &Path, external: &ExternalConfig, reason: String) {
        let message = refused_plugin(name, &reason);
        let mut parked = self.parked();
        if parked.get(name).is_some_and(|p| p.info.message == message) {
            return;
        }
        log::info!("{message}");
        let mut config = external.module(name, path);
        config.capabilities.apply_limits(&self.0.limits);
        let info = InactiveModule {
            name: name.to_string(),
            path: Some(path.to_path_buf()),
            external: true,
            sha256: std::fs::read(path).ok().map(|bytes| sha256(&bytes)),
            reason: InactiveReason::Denied,
            message,
            capabilities: config.capabilities.clone(),
        };
        parked.insert(
            name.to_string(),
            Parked {
                config,
                retry: None,
                info,
            },
        );
    }

    /// Forward subscribed app events to the module.
    fn subscribe(&self, module: &Arc<Module<P>>) {
        let mut ids = module.listeners.lock().unwrap_or_else(|e| e.into_inner());
        for event in &module.config.capabilities.events.listen {
            let backend = Arc::downgrade(&self.0);
            let name = module.name.clone();
            let event_name = event.clone();
            let handler = move |payload: Value| {
                let Some(inner) = backend.upgrade() else {
                    return;
                };
                let backend = Backend(inner.clone());
                let name = name.clone();
                let event = event_name.clone();
                // Never run WASM inline: the emitter may be the module itself.
                inner.platform.spawn_blocking(Box::new(move || {
                    if let Err(err) = backend.send_event(&name, &event, &payload) {
                        log::error!("WASM module `{name}` failed to handle event `{event}`: {err}");
                    }
                }));
            };
            ids.push(self.0.platform.listen(event, Box::new(handler)));
        }
    }

    fn resolve_module_path(&self, path: &Path, external: bool) -> Result<PathBuf> {
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        if external || self.0.dev {
            // External plugin paths are already absolute; dev paths are
            // relative to the working directory (`src-tauri` under `tauri dev`).
            return Ok(std::env::current_dir()?.join(path));
        }
        self.0.platform.resolve(path, BaseDir::Resource)
    }
}

impl<P: Platform> Backend<P> {
    fn resolve_capabilities(&self, caps: &Capabilities) -> Result<Capabilities> {
        let mut caps = caps.clone();
        let default = BaseDir::AppData;
        caps.wasi.preopens = caps
            .wasi
            .preopens
            .iter()
            .map(|p| {
                let host = resolve_dir(&self.0.platform, &p.host, default)?;
                if !p.read_only {
                    let _ = std::fs::create_dir_all(&host);
                }
                Ok(Preopen { host, ..p.clone() })
            })
            .collect::<Result<_>>()?;
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_arguments_is_null_or_empty_object() {
        #[derive(Deserialize, Debug, PartialEq, Default)]
        #[serde(default)]
        struct Opts {
            verbose: bool,
        }
        assert!(deserialize_args::<()>(json!(null)).is_ok());
        assert!(deserialize_args::<()>(json!({})).is_ok());
        assert_eq!(
            deserialize_args::<Opts>(json!(null)).unwrap(),
            Opts::default()
        );
        assert_eq!(deserialize_args::<Option<u32>>(json!({})).unwrap(), None);
        // Real arguments are left alone, errors included.
        assert!(deserialize_args::<()>(json!({ "x": 1 })).is_err());
        let e = deserialize_args::<u32>(json!(null))
            .unwrap_err()
            .to_string();
        assert!(e.contains("invalid type: null"), "{e}");
    }
}
