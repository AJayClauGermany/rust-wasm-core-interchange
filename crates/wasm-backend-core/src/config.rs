//! Backend configuration. With Tauri it is read from `plugins.wasm-backend`
//! in `tauri.conf.json` and/or set through the plugin's `Builder`; other
//! integrations can deserialize it from any format or build it in code.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::capabilities::{Capabilities, Limits, wildcard_match};

/// Which runtime executes a module.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum EngineKind {
    /// Detect from the binary: WebAssembly components, and core modules
    /// carrying component type information (built against a WIT world for
    /// `wasm32-unknown-unknown`), use the WIT engine; other core modules use
    /// Extism.
    #[default]
    Auto,
    /// WebAssembly component model (WIT) via wasmtime.
    Component,
    /// Extism plugin.
    Extism,
}

/// Configuration of the whole backend.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// Modules shipped with (or developed alongside) the app, keyed by name.
    #[serde(default)]
    pub modules: BTreeMap<String, ModuleConfig>,
    /// Third-party plugins loaded from a directory at runtime.
    #[serde(default)]
    pub external: Option<ExternalConfig>,
    /// Resource limits for every module that doesn't set its own, external
    /// plugins and modules loaded at runtime included.
    #[serde(default)]
    pub limits: Limits,
    /// Watch module files and reload them when they change.
    /// Defaults to `true` in debug builds and `false` in release builds.
    #[serde(default)]
    pub hot_reload: Option<bool>,
    /// In debug builds, write TypeScript bindings for every module to
    /// `<dir>/<module>.ts` on (re)load. Relative to the current directory
    /// (`src-tauri` under `tauri dev`). Applied by the Tauri plugin; other
    /// integrations use [`BackendBuilder::typescript_out_dir`](crate::BackendBuilder::typescript_out_dir).
    #[serde(default)]
    pub typescript_out_dir: Option<PathBuf>,
    /// Directory module state is kept in between runs, one JSON file per
    /// module. May start with `$APPDATA` and the like. Applied by the Tauri
    /// plugin (default: `$APPDATA/wasm-backend/state`); other integrations
    /// use [`BackendBuilder::state_store`](crate::BackendBuilder::state_store).
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
}

/// Configuration of a single module.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModuleConfig {
    /// Path of the `.wasm` file. Relative paths are resolved against the
    /// app's resource directory ([`BaseDir::Resource`](crate::BaseDir)) in
    /// release builds and against the current directory (`src-tauri` under
    /// `tauri dev`) in debug builds.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Path used instead of `path` in debug builds.
    #[serde(default)]
    pub dev_path: Option<PathBuf>,
    #[serde(default)]
    pub engine: EngineKind,
    /// What the module is allowed to do.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Whether the frontend may invoke this module when the `invoke` command
    /// is allowed without an explicit scope (see [`ModuleInfo::exposed`](crate::ModuleInfo::exposed)). Defaults to `true` for app
    /// modules and `false` for external plugins.
    #[serde(default)]
    pub expose: Option<bool>,
    /// Override the global hot reload setting for this module.
    #[serde(default)]
    pub watch: Option<bool>,
    /// `false` keeps the module from being loaded until
    /// [`Backend::enable`](crate::Backend::enable) is called. It stays
    /// listed in [`Backend::inactive`](crate::Backend::inactive).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Only load the module if this is the SHA-256 of its bytes (hex). A
    /// file with any other content is refused, on reload too.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Module bytes, when the module is embedded in the binary instead of
    /// being loaded from disk (see [`ModuleConfig::from_bytes`]).
    #[serde(skip)]
    pub bytes: Option<Arc<[u8]>>,
}

impl ModuleConfig {
    /// A module loaded from a file.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            ..Default::default()
        }
    }

    /// A module embedded in the binary, e.g. with `include_bytes!`.
    /// Embedded modules can't be hot reloaded.
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: Some(bytes.into()),
            ..Default::default()
        }
    }

    /// Path used in debug builds.
    pub fn dev_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.dev_path = Some(path.into());
        self
    }

    pub fn engine(mut self, engine: EngineKind) -> Self {
        self.engine = engine;
        self
    }

    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn expose(mut self, expose: bool) -> Self {
        self.expose = Some(expose);
        self
    }

    pub fn watch(mut self, watch: bool) -> Self {
        self.watch = Some(watch);
        self
    }

    /// `false` keeps the module from being loaded until
    /// [`Backend::enable`](crate::Backend::enable) is called.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// Only load the module if this is the SHA-256 of its bytes (hex).
    pub fn sha256(mut self, sha256: impl Into<String>) -> Self {
        self.sha256 = Some(sha256.into());
        self
    }

    /// The configured path, unresolved: `dev_path` (falling back to `path`)
    /// in development, `path` otherwise.
    pub fn configured_path(&self, dev: bool) -> Option<&Path> {
        if dev {
            self.dev_path.as_deref().or(self.path.as_deref())
        } else {
            self.path.as_deref()
        }
    }
}

/// Third-party plugins: every `*.wasm` file in `dir` is loaded as a module
/// named after the file stem.
///
/// ```
/// use wasm_backend_core::{Capabilities, ExternalConfig, ExternalPluginConfig};
///
/// let external = ExternalConfig::new("$APPDATA/plugins")
///     // What every plugin gets...
///     .capabilities(Capabilities::default().allow_emit("plugin://*"))
///     // ...and what one of them gets instead.
///     .plugin(
///         "weather",
///         ExternalPluginConfig::default()
///             .capabilities(Capabilities::default().allow_host("api.weather.example")),
///     )
///     .deny("legacy-*");
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalConfig {
    /// Directory to load plugins from. Relative paths are resolved against
    /// the app data directory.
    pub dir: PathBuf,
    /// Capabilities granted to every external plugin. Defaults to none.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Whether the frontend may invoke external plugins without an explicit
    /// `invoke` scope entry naming them. Defaults to `false`.
    #[serde(default)]
    pub expose: bool,
    /// Engine used for external plugins.
    #[serde(default)]
    pub engine: EngineKind,
    /// Names of the plugins that may be loaded (`*` wildcards). Empty
    /// allows every plugin `deny` doesn't match.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Names of the plugins that are never loaded (`*` wildcards). Wins
    /// over `allow`.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Settings for single plugins, by name, replacing the shared ones
    /// above.
    #[serde(default)]
    pub plugins: BTreeMap<String, ExternalPluginConfig>,
}

/// What one external plugin gets instead of the settings shared by all of
/// them. Unset fields keep the shared setting.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalPluginConfig {
    /// Capabilities of this plugin, replacing (not adding to) the shared
    /// ones.
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
    #[serde(default)]
    pub expose: Option<bool>,
    #[serde(default)]
    pub engine: Option<EngineKind>,
    /// `false` keeps the plugin from being loaded until
    /// [`Backend::enable`](crate::Backend::enable) is called.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Only load the plugin if this is the SHA-256 of its file (hex).
    #[serde(default)]
    pub sha256: Option<String>,
}

impl ExternalPluginConfig {
    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    pub fn expose(mut self, expose: bool) -> Self {
        self.expose = Some(expose);
        self
    }

    pub fn engine(mut self, engine: EngineKind) -> Self {
        self.engine = Some(engine);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    pub fn sha256(mut self, sha256: impl Into<String>) -> Self {
        self.sha256 = Some(sha256.into());
        self
    }
}

impl ExternalConfig {
    /// Load plugins from `dir`, with no capabilities.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            ..Default::default()
        }
    }

    /// Capabilities granted to every plugin.
    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn expose(mut self, expose: bool) -> Self {
        self.expose = expose;
        self
    }

    pub fn engine(mut self, engine: EngineKind) -> Self {
        self.engine = engine;
        self
    }

    /// Only load plugins whose name matches `pattern` (or another allowed
    /// one).
    pub fn allow(mut self, pattern: impl Into<String>) -> Self {
        self.allow.push(pattern.into());
        self
    }

    /// Never load plugins whose name matches `pattern`.
    pub fn deny(mut self, pattern: impl Into<String>) -> Self {
        self.deny.push(pattern.into());
        self
    }

    /// Settings for the plugin called `name`.
    pub fn plugin(mut self, name: impl Into<String>, config: ExternalPluginConfig) -> Self {
        self.plugins.insert(name.into(), config);
        self
    }

    /// Why the plugin called `name` may not be loaded, if it may not.
    pub(crate) fn refusal(&self, name: &str) -> Option<String> {
        if let Some(pattern) = self.deny.iter().find(|p| wildcard_match(p, name)) {
            return Some(format!("`external.deny` lists `{pattern}`"));
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|p| wildcard_match(p, name)) {
            return Some("`external.allow` doesn't list it".to_string());
        }
        None
    }

    /// The configuration the plugin `name` at `path` is loaded with.
    pub(crate) fn module(&self, name: &str, path: &Path) -> ModuleConfig {
        let plugin = self.plugins.get(name).cloned().unwrap_or_default();
        ModuleConfig {
            path: Some(path.to_path_buf()),
            dev_path: Some(path.to_path_buf()),
            engine: plugin.engine.unwrap_or(self.engine),
            capabilities: plugin
                .capabilities
                .unwrap_or_else(|| self.capabilities.clone()),
            expose: Some(plugin.expose.unwrap_or(self.expose)),
            enabled: plugin.enabled,
            sha256: plugin.sha256,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_plugins_are_allowed_and_denied_by_name() {
        let open = ExternalConfig::new("plugins").deny("legacy-*");
        assert_eq!(open.refusal("weather"), None);
        assert!(open.refusal("legacy-sync").unwrap().contains("`legacy-*`"));

        let closed = ExternalConfig::new("plugins")
            .allow("official-*")
            .deny("official-beta");
        assert_eq!(closed.refusal("official-notes"), None);
        assert!(closed.refusal("weather").unwrap().contains("allow"));
        assert!(closed.refusal("official-beta").unwrap().contains("deny"));
    }

    #[test]
    fn a_plugin_replaces_the_shared_settings() {
        let external: ExternalConfig = serde_json::from_value(serde_json::json!({
            "dir": "plugins",
            "capabilities": { "events": { "emit": ["plugin://*"] } },
            "plugins": {
                "weather": { "capabilities": { "allowedHosts": ["api.weather.example"] }, "expose": true },
                "notes": { "enabled": false }
            }
        }))
        .unwrap();
        let path = Path::new("/plugins/x.wasm");
        let other = external.module("other", path);
        assert_eq!(other.capabilities.events.emit, ["plugin://*"]);
        assert_eq!(other.expose, Some(false));

        let weather = external.module("weather", path);
        assert!(weather.capabilities.events.emit.is_empty());
        assert_eq!(weather.capabilities.allowed_hosts, ["api.weather.example"]);
        assert_eq!(weather.expose, Some(true));

        let notes = external.module("notes", path);
        assert_eq!(notes.enabled, Some(false));
        assert_eq!(notes.capabilities.events.emit, ["plugin://*"]);
    }
}
