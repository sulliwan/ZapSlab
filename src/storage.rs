//! localStorage autosave for the editor.
//!
//! The key is versioned so the storage format can change without migrations:
//! an older version is simply ignored and overwritten.

#[cfg(target_arch = "wasm32")]
mod imp {
    use web_sys::{window, Storage};

    const KEY: &str = "boopweb.text.v1";

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
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub fn load() -> Option<String> {
        None
    }

    pub fn save(_text: &str) -> Result<(), ()> {
        Ok(())
    }
}

pub use imp::{load, save};
