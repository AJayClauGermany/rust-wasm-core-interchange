//! What a WASM module is allowed to do.
//!
//! Capabilities are deny-by-default: a module with `Capabilities::default()`
//! can compute and log, but it can't emit or listen to events, call host
//! commands, touch the filesystem, the network or the environment.
//!
//! The pattern lists (`hostCommands`, `events.emit`, `state`) grant with
//! plain entries, where `*` matches anything, and take back with entries
//! starting with `!`: `["db_*", "!db_drop"]` allows every `db_` command but
//! `db_drop`. A `!` entry always wins, wherever it is in the list.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Capabilities granted to a module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Capabilities {
    /// Host commands (registered with `Builder::host_command`) the module may
    /// invoke. Supports `*` wildcards and `!` entries that deny.
    #[serde(default)]
    pub host_commands: Vec<String>,
    /// Events the module may emit and receive.
    #[serde(default)]
    pub events: EventCapabilities,
    /// Keys of its own state store the module may read and write through
    /// `tauri:backend/state` (`tauri_state_*` for Extism). Supports `*`
    /// wildcards and `!` entries that deny; `["*"]` grants the whole store.
    #[serde(default)]
    pub state: Vec<String>,
    /// WASI access (filesystem, environment, stdio, sockets).
    #[serde(default)]
    pub wasi: WasiCapabilities,
    /// Hosts the module may reach with Extism's HTTP functions. Supports `*`
    /// wildcards (e.g. `*.example.com`). Extism only.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Maximum linear memory, in MiB. When unset, the backend's
    /// [`Limits`] apply.
    #[serde(default)]
    pub memory_limit_mb: Option<u32>,
    /// Maximum wall-clock time for a single call, in milliseconds. When
    /// unset, the backend's [`Limits`] apply.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// How many times the module is started again after a call traps (a
    /// panic, a timeout, running out of memory). The trap after the last
    /// restart stops the module: every call fails with
    /// [`Error::Stopped`](crate::Error::Stopped) until it is reloaded.
    /// Counted from when the module was last (re)loaded. When unset, the
    /// backend's [`Limits`] apply; unlimited without them.
    #[serde(default)]
    pub max_restarts: Option<u32>,
}

/// Resource limits for every module that doesn't set its own: the general
/// counterpart of the three limits in [`Capabilities`]. Set them once with
/// [`BackendBuilder::limits`](crate::BackendBuilder::limits) (`limits` in
/// [`Config`](crate::Config)) and they also cover external plugins and
/// modules loaded at runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// Maximum linear memory, in MiB.
    #[serde(default)]
    pub memory_limit_mb: Option<u32>,
    /// Maximum wall-clock time for a single call, in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Restarts after a trap before the module is stopped (see
    /// [`Capabilities::max_restarts`]).
    #[serde(default)]
    pub max_restarts: Option<u32>,
}

impl Limits {
    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    pub fn memory_limit_mb(mut self, mb: u32) -> Self {
        self.memory_limit_mb = Some(mb);
        self
    }

    pub fn max_restarts(mut self, restarts: u32) -> Self {
        self.max_restarts = Some(restarts);
        self
    }
}

/// Event related capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventCapabilities {
    /// Event names the module may emit. Supports `*` wildcards and `!`
    /// entries that deny.
    #[serde(default)]
    pub emit: Vec<String>,
    /// Exact event names the module subscribes to. They are delivered to the
    /// module's `on-event` export (`on_event` for Extism).
    #[serde(default)]
    pub listen: Vec<String>,
}

/// WASI related capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WasiCapabilities {
    /// Forward the module's stdout/stderr to the app's stdout/stderr.
    #[serde(default)]
    pub stdio: bool,
    /// Environment variables visible to the module.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Host directories made available to the module.
    #[serde(default)]
    pub preopens: Vec<Preopen>,
    /// Allow outgoing/incoming TCP and UDP sockets and DNS lookups (WIT only).
    #[serde(default)]
    pub network: bool,
}

/// A host directory mapped into the module's filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Preopen {
    /// Host directory. May start with `$APPDATA`, `$APPLOCALDATA`,
    /// `$APPCACHE`, `$APPCONFIG`, `$RESOURCE` or `$TEMP` (resolved by the
    /// [`Platform`](crate::Platform)); other relative paths are resolved
    /// against the app data directory. Writable
    /// directories are created if missing.
    pub host: PathBuf,
    /// Path the directory is visible under inside the module.
    pub guest: String,
    #[serde(default)]
    pub read_only: bool,
}

impl Preopen {
    /// Make the host directory `host` visible to the module as `guest`,
    /// writable.
    pub fn new(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            read_only: false,
        }
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }
}

impl Capabilities {
    /// Let the module invoke the host commands matching `pattern` (`*` for
    /// all of them).
    pub fn allow_host_command(mut self, pattern: impl Into<String>) -> Self {
        self.host_commands.push(pattern.into());
        self
    }

    /// Keep the module from invoking the host commands matching `pattern`,
    /// whatever else is allowed: `allow_host_command("db_*")` with
    /// `deny_host_command("db_drop")`.
    pub fn deny_host_command(mut self, pattern: impl AsRef<str>) -> Self {
        self.host_commands.push(deny(pattern));
        self
    }

    /// Let the module emit the events matching `pattern`.
    pub fn allow_emit(mut self, pattern: impl Into<String>) -> Self {
        self.events.emit.push(pattern.into());
        self
    }

    /// Keep the module from emitting the events matching `pattern`,
    /// whatever else is allowed.
    pub fn deny_emit(mut self, pattern: impl AsRef<str>) -> Self {
        self.events.emit.push(deny(pattern));
        self
    }

    pub fn listen(mut self, event: impl Into<String>) -> Self {
        self.events.listen.push(event.into());
        self
    }

    /// Let the module read and write the keys of its state store matching
    /// `pattern` (`*` for all of it).
    pub fn allow_state(mut self, pattern: impl Into<String>) -> Self {
        self.state.push(pattern.into());
        self
    }

    /// Keep the module away from the keys of its state store matching
    /// `pattern`, whatever else is allowed.
    pub fn deny_state(mut self, pattern: impl AsRef<str>) -> Self {
        self.state.push(deny(pattern));
        self
    }

    /// Forward the module's stdout/stderr to the app's.
    pub fn stdio(mut self, enabled: bool) -> Self {
        self.wasi.stdio = enabled;
        self
    }

    /// Set an environment variable the module sees.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.wasi.env.insert(name.into(), value.into());
        self
    }

    /// Make a host directory available to the module: `Preopen::new(host,
    /// guest)`, with `.read_only(true)` to keep the module from writing.
    pub fn preopen(mut self, preopen: Preopen) -> Self {
        self.wasi.preopens.push(preopen);
        self
    }

    /// Allow TCP and UDP sockets and DNS lookups (WIT modules only).
    pub fn network(mut self, enabled: bool) -> Self {
        self.wasi.network = enabled;
        self
    }

    /// Let the module reach the hosts matching `pattern` with Extism's HTTP
    /// functions (Extism modules only).
    pub fn allow_host(mut self, pattern: impl Into<String>) -> Self {
        self.allowed_hosts.push(pattern.into());
        self
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    pub fn memory_limit_mb(mut self, mb: u32) -> Self {
        self.memory_limit_mb = Some(mb);
        self
    }

    /// Stop the module once it has trapped more than `restarts` times since
    /// it was (re)loaded, instead of starting it again forever.
    pub fn max_restarts(mut self, restarts: u32) -> Self {
        self.max_restarts = Some(restarts);
        self
    }

    /// Fill the limits this module doesn't set with the backend's.
    pub(crate) fn apply_limits(&mut self, limits: &Limits) {
        self.memory_limit_mb = self.memory_limit_mb.or(limits.memory_limit_mb);
        self.timeout_ms = self.timeout_ms.or(limits.timeout_ms);
        self.max_restarts = self.max_restarts.or(limits.max_restarts);
    }

    /// Host command names match across naming conventions, like module
    /// commands do (`app_version` allows `app-version` and `appVersion`).
    pub(crate) fn can_invoke(&self, command: &str) -> bool {
        allows(&self.host_commands, command, name_matches)
    }

    pub(crate) fn can_store(&self, key: &str) -> bool {
        any_match(&self.state, key)
    }

    pub(crate) fn can_emit(&self, event: &str) -> bool {
        any_match(&self.events.emit, event)
    }
}

/// The entry that denies `pattern`.
fn deny(pattern: impl AsRef<str>) -> String {
    format!("!{}", pattern.as_ref())
}

/// Whether a pattern list grants `value`: an entry matches it and no `!`
/// entry does.
fn allows(patterns: &[String], value: &str, matches: fn(&str, &str) -> bool) -> bool {
    let mut allowed = false;
    for pattern in patterns {
        match pattern.strip_prefix('!') {
            Some(denied) if matches(denied, value) => return false,
            Some(_) => {}
            None => allowed |= matches(pattern, value),
        }
    }
    allowed
}

pub(crate) fn any_match(patterns: &[String], value: &str) -> bool {
    allows(patterns, value, wildcard_match)
}

/// Match a command name against a pattern from a capability or scope.
/// Patterns with `*` are matched literally; plain names match across
/// `kebab-case`, `snake_case` and `camelCase` (`get_user` matches `getUser`).
pub fn name_matches(pattern: &str, name: &str) -> bool {
    wildcard_match(pattern, name)
        || (!pattern.contains('*')
            && crate::naming::to_kebab(pattern) == crate::naming::to_kebab(name))
}

/// Glob-like matching where `*` matches any (possibly empty) sequence.
pub fn wildcard_match(pattern: &str, value: &str) -> bool {
    let p = pattern.as_bytes();
    let v = value.as_bytes();
    let (mut pi, mut vi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while vi < v.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, vi));
            pi += 1;
        } else if pi < p.len() && p[pi] == v[vi] {
            pi += 1;
            vi += 1;
        } else if let Some((sp, sv)) = star {
            pi = sp + 1;
            vi = sv + 1;
            star = Some((sp, sv + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("greeter://*", "greeter://greeted"));
        assert!(!wildcard_match("greeter://*", "other://greeted"));
        assert!(wildcard_match("a*c*e", "abcde"));
        assert!(!wildcard_match("a*c*e", "abcdf"));
        assert!(wildcard_match("exact", "exact"));
        assert!(!wildcard_match("exact", "exactly"));
    }

    #[test]
    fn deny_by_default() {
        let caps = Capabilities::default();
        assert!(!caps.can_emit("x"));
        assert!(!caps.can_invoke("x"));
        let caps = caps.allow_emit("app://*").allow_host_command("now");
        assert!(caps.can_emit("app://tick"));
        assert!(caps.can_invoke("now"));
        assert!(!caps.can_invoke("now2"));
    }

    #[test]
    fn deny_entries_win() {
        let caps = Capabilities::default()
            .deny_host_command("db_drop")
            .allow_host_command("db_*")
            .allow_emit("*")
            .deny_emit("admin://*")
            .allow_state("*")
            .deny_state("secret.*");
        assert_eq!(caps.host_commands, ["!db_drop", "db_*"]);
        assert!(caps.can_invoke("db_query"));
        // Whatever the order, and across naming conventions.
        assert!(!caps.can_invoke("db_drop"));
        assert!(!caps.can_invoke("dbDrop"));
        assert!(caps.can_emit("app://tick"));
        assert!(!caps.can_emit("admin://reset"));
        assert!(caps.can_store("theme"));
        assert!(!caps.can_store("secret.token"));
        // Denying alone grants nothing.
        assert!(!Capabilities::default().deny_emit("x").can_emit("y"));
    }

    #[test]
    fn limits_fill_what_a_module_leaves_unset() {
        let limits = Limits::default().timeout_ms(5000).max_restarts(3);
        let mut caps = Capabilities::default().timeout_ms(100);
        caps.apply_limits(&limits);
        assert_eq!(caps.timeout_ms, Some(100));
        assert_eq!(caps.max_restarts, Some(3));
        assert_eq!(caps.memory_limit_mb, None);
    }

    #[test]
    fn builders_cover_wasi() {
        let caps = Capabilities::default()
            .env("MODE", "dev")
            .network(true)
            .allow_host("*.example.com")
            .preopen(Preopen::new("$APPDATA/x", "/data").read_only(true));
        assert_eq!(caps.wasi.env["MODE"], "dev");
        assert!(caps.wasi.network);
        assert_eq!(caps.allowed_hosts, ["*.example.com"]);
        assert!(caps.wasi.preopens[0].read_only);
    }

    #[test]
    fn host_command_names_match_across_conventions() {
        let caps = Capabilities::default()
            .allow_host_command("app_version")
            .allow_host_command("db_*");
        assert!(caps.can_invoke("app-version"));
        assert!(caps.can_invoke("appVersion"));
        assert!(!caps.can_invoke("app_versions"));
        // Wildcards stay literal, so they never widen to other spellings.
        assert!(caps.can_invoke("db_query"));
        assert!(!caps.can_invoke("dbAdmin"));
    }
}
