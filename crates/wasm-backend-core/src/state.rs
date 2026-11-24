//! Where module state is kept between runs of the app.
//!
//! A module's memory is reset whenever it is reloaded or traps. What it
//! stores through the `tauri:backend/state` interface (`tauri_state_*` for
//! Extism) is kept by the host instead: it survives both, and with a
//! [`StateStore`] such as [`FileStateStore`] it survives the app as well.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Longest key a module may store a value under, in bytes.
pub const STATE_KEY_MAX: usize = 256;
/// Longest value a module may store, in bytes.
pub const STATE_VALUE_MAX: usize = 64 * 1024;

/// Persists module state. The backend keeps every loaded module's state in
/// memory and calls the store to read it once and to write it after each
/// call that changed it. Set one with
/// [`BackendBuilder::state_store`](crate::BackendBuilder::state_store);
/// without one, state lasts as long as the [`Backend`](crate::Backend).
pub trait StateStore: Send + Sync + 'static {
    /// Everything stored for `module`; empty if nothing was.
    fn load(&self, module: &str) -> Result<BTreeMap<String, String>, String>;
    /// Replace what is stored for `module` with `state`.
    fn save(&self, module: &str, state: &BTreeMap<String, String>) -> Result<(), String>;
}

/// Keeps each module's state in a JSON file, `<dir>/<module>.json`. Files
/// are replaced atomically, so a crash never leaves half a write behind.
#[derive(Debug, Clone)]
pub struct FileStateStore {
    dir: PathBuf,
}

impl FileStateStore {
    /// Store state in `dir`, which is created when first written to.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory state is stored in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file holding `module`'s state. Characters that could leave the
    /// directory or collide on some file systems are percent-encoded.
    pub fn path(&self, module: &str) -> PathBuf {
        let mut name = String::with_capacity(module.len() + 5);
        for (i, b) in module.bytes().enumerate() {
            let plain = b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'-' | b'_')
                || (b == b'.' && i > 0);
            if plain {
                name.push(b as char);
            } else {
                name.push_str(&format!("%{b:02X}"));
            }
        }
        name.push_str(".json");
        self.dir.join(name)
    }
}

impl StateStore for FileStateStore {
    fn load(&self, module: &str) -> Result<BTreeMap<String, String>, String> {
        let path = self.path(module);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} is not a state file: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(format!("can't read {}: {e}", path.display())),
        }
    }

    fn save(&self, module: &str, state: &BTreeMap<String, String>) -> Result<(), String> {
        let path = self.path(module);
        let failed = |e: std::io::Error| format!("can't write {}: {e}", path.display());
        if state.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(failed(e)),
                _ => Ok(()),
            };
        }
        let json = serde_json::to_vec(state).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.dir).map_err(failed)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(failed)?;
        std::fs::rename(&tmp, &path).map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trips_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("wasm-core-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileStateStore::new(&dir);
        assert!(store.load("greeter").unwrap().is_empty());
        assert!(!dir.exists());

        let state = BTreeMap::from([("a".to_string(), "1".to_string())]);
        store.save("greeter", &state).unwrap();
        assert_eq!(store.load("greeter").unwrap(), state);
        assert!(store.load("other").unwrap().is_empty());

        store.save("greeter", &BTreeMap::new()).unwrap();
        assert!(!store.path("greeter").exists());
        store.save("greeter", &BTreeMap::new()).unwrap();

        std::fs::write(store.path("broken"), "[]").unwrap();
        assert!(
            store
                .load("broken")
                .unwrap_err()
                .contains("not a state file")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn module_names_can_not_leave_the_directory() {
        let store = FileStateStore::new("state");
        let name = |m: &str| {
            store
                .path(m)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(name("my-plugin_2.x"), "my-plugin_2.x.json");
        assert_eq!(name("../evil"), "%2E.%2Fevil.json");
        assert_eq!(name(".."), "%2E..json");
        assert_eq!(name("a\\b:c"), "a%5Cb%3Ac.json");
        // Names differing only in case stay apart on case-insensitive disks.
        assert_ne!(name("Greeter"), name("greeter"));
        assert_eq!(store.path("../evil").parent().unwrap(), Path::new("state"));
    }
}
