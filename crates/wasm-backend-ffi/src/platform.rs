//! The [`Platform`] a C host runs on: events go to a callback and an
//! in-process bus, directories come from the configuration.

use std::{
    ffi::{CString, c_char, c_void},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use wasm_backend_core::{BaseDir, Config, Error, EventBus, EventHandler, Platform, Result};

/// A string for C. Interior NULs can't be represented and are dropped.
pub(crate) fn c_string(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| {
        CString::new(s.replace('\0', "")).expect("the string has no NUL bytes any more")
    })
}

/// The host's event callback and what it wants passed back.
#[derive(Clone, Copy)]
pub(crate) struct EventSink {
    pub f: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char),
    pub user_data: *mut c_void,
}

// The host promises that its callbacks and their `user_data` may be used
// from any thread (see the crate documentation).
unsafe impl Send for EventSink {}
unsafe impl Sync for EventSink {}

/// The standard directories, as far as the host configured them.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Dirs {
    app_data: Option<PathBuf>,
    app_local_data: Option<PathBuf>,
    app_cache: Option<PathBuf>,
    app_config: Option<PathBuf>,
    resource: Option<PathBuf>,
    temp: Option<PathBuf>,
}

/// What the builder is configured with: the core's [`Config`] plus what a
/// framework would otherwise provide.
#[derive(Debug)]
pub(crate) struct Options {
    pub config: Config,
    pub dirs: Dirs,
    pub dev: bool,
}

impl Options {
    pub fn parse(json: Value) -> std::result::Result<Self, String> {
        let mut json = match json {
            Value::Null => serde_json::Map::new(),
            Value::Object(map) => map,
            _ => return Err("the configuration must be a JSON object".to_string()),
        };
        let invalid = |key: &str, e: serde_json::Error| format!("invalid `{key}`: {e}");
        let dirs = match json.remove("dirs") {
            Some(dirs) => serde_json::from_value(dirs).map_err(|e| invalid("dirs", e))?,
            None => Dirs::default(),
        };
        let dev = match json.remove("dev") {
            Some(dev) => serde_json::from_value(dev).map_err(|e| invalid("dev", e))?,
            None => false,
        };
        let config = serde_json::from_value(Value::Object(json))
            .map_err(|e| format!("invalid configuration: {e}"))?;
        Ok(Self { config, dirs, dev })
    }
}

pub(crate) struct FfiPlatform {
    pub bus: EventBus,
    sink: Option<EventSink>,
    dirs: Dirs,
}

impl FfiPlatform {
    pub fn new(dirs: Dirs, sink: Option<EventSink>) -> Self {
        Self {
            bus: EventBus::new(),
            sink,
            dirs,
        }
    }
}

impl Platform for FfiPlatform {
    type ListenerId = u64;

    fn emit(&self, event: &str, payload: Value) -> std::result::Result<(), String> {
        if let Some(sink) = &self.sink {
            let (name, json) = (c_string(event), c_string(&payload.to_string()));
            // SAFETY: the host registered this callback for these arguments,
            // which stay valid for the duration of the call.
            unsafe { (sink.f)(sink.user_data, name.as_ptr(), json.as_ptr()) };
        }
        // Modules listening to it.
        self.bus.emit(event, &payload);
        Ok(())
    }

    fn listen(&self, event: &str, handler: EventHandler) -> u64 {
        self.bus.listen(event, handler)
    }

    fn unlisten(&self, id: u64) {
        self.bus.unlisten(id)
    }

    fn base_dir(&self, base: BaseDir) -> Result<PathBuf> {
        let dirs = &self.dirs;
        let dir = match base {
            BaseDir::AppData => dirs.app_data.clone(),
            BaseDir::AppLocalData => dirs.app_local_data.clone().or(dirs.app_data.clone()),
            BaseDir::AppCache => dirs.app_cache.clone().or(dirs.app_data.clone()),
            BaseDir::AppConfig => dirs.app_config.clone().or(dirs.app_data.clone()),
            BaseDir::Resource => Some(dirs.resource.clone().unwrap_or_default()),
            BaseDir::Temp => Some(dirs.temp.clone().unwrap_or_else(std::env::temp_dir)),
        };
        let dir = dir.ok_or_else(|| {
            Error::platform(format!(
                "a path uses {} but the configuration has no `dirs.appData`",
                base.variable()
            ))
        })?;
        absolute(&dir)
    }
}

/// Relative directories are relative to the current one.
fn absolute(dir: &Path) -> Result<PathBuf> {
    if dir.is_absolute() {
        Ok(dir.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn options_are_the_core_config_plus_dirs() {
        let options = Options::parse(json!({
            "modules": { "greeter": { "path": "greeter.wasm" } },
            "limits": { "timeoutMs": 1000 },
            "dirs": { "appData": "/data" },
            "dev": true
        }))
        .unwrap();
        assert!(options.dev);
        assert!(options.config.modules.contains_key("greeter"));
        assert_eq!(options.config.limits.timeout_ms, Some(1000));

        let platform = FfiPlatform::new(options.dirs, None);
        let dir = |base| platform.base_dir(base).unwrap();
        assert_eq!(dir(BaseDir::AppData), PathBuf::from("/data"));
        // Directories that aren't set fall back to sensible ones.
        assert_eq!(dir(BaseDir::AppCache), PathBuf::from("/data"));
        assert_eq!(dir(BaseDir::Resource), std::env::current_dir().unwrap());
        assert_eq!(dir(BaseDir::Temp), std::env::temp_dir());

        assert!(Options::parse(Value::Null).is_ok());
        let typo = Options::parse(json!({ "module": {} })).unwrap_err();
        assert!(typo.contains("unknown field `module`"), "{typo}");
        let typo = Options::parse(json!({ "dirs": { "data": "x" } })).unwrap_err();
        assert!(typo.contains("invalid `dirs`"), "{typo}");

        let nowhere = FfiPlatform::new(Dirs::default(), None);
        let e = nowhere.base_dir(BaseDir::AppData).unwrap_err().to_string();
        assert!(e.contains("$APPDATA") && e.contains("dirs.appData"), "{e}");
    }
}
