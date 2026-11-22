//! localStorage autosave for the editor.
//!
//! The key is versioned so the storage format can change without migrations:
//! an older version is simply ignored and overwritten.

#[cfg(target_arch = "wasm32")]
mod imp {
    use web_sys::{window, Storage};

    const KEY: &str = "zapslab.text.v1";
    /// Most-recently-used transform names, newest first. Versioned like the
    /// text key: an older/malformed value is ignored rather than migrated.
    const RECENT_KEY: &str = "zapslab.recent.v1";

    fn storage() -> Option<Storage> {
        window()?.local_storage().ok().flatten()
    }

    pub fn load() -> Option<String> {
        storage()?.get_item(KEY).ok().flatten()
    }

    /// Persists `text`. Returns `Err(())` when the write fails — for example a
    /// `QuotaExceededError` on a large document, or storage being unavailable
    /// or blocked. Callers surface this instead of pretending the save worked.
    pub fn save(text: &str) -> Result<(), ()> {
        let storage = storage().ok_or(())?;
        storage.set_item(KEY, text).map_err(|_| ())
    }

    pub fn load_recent() -> Vec<String> {
        let Some(raw) = storage().and_then(|storage| storage.get_item(RECENT_KEY).ok().flatten())
        else {
            return Vec::new();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save_recent(names: &[String]) -> Result<(), ()> {
        let storage = storage().ok_or(())?;
        let encoded = serde_json::to_string(names).map_err(|_| ())?;
        storage.set_item(RECENT_KEY, &encoded).map_err(|_| ())
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub fn load() -> Option<String> {
        None
    }

    pub fn save(_text: &str) -> Result<(), ()> {
        Ok(())
    }

    pub fn load_recent() -> Vec<String> {
        Vec::new()
    }

    pub fn save_recent(_names: &[String]) -> Result<(), ()> {
        Ok(())
    }
}

pub use imp::{load, load_recent, save, save_recent};
