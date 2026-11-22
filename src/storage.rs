//! Persistence for the editor.
//!
//! Document text lives in **IndexedDB** so up to the 64 MB cap can be
//! autosaved (localStorage's ~5–10 MB quota cannot hold that). IndexedDB is
//! asynchronous, so `load`/`save` are `async` on both targets; the non-wasm
//! stub returns immediately.
//!
//! The tiny, recently-used transform list, the recent find queries, and the
//! last find/replace dialog state stay in localStorage: they are well under
//! quota and synchronous, which keeps the palette and find dialog snappy.
//!
//! Keys are versioned so the storage format can change without migrations: an
//! older version is ignored and overwritten. The legacy localStorage text key
//! (`zapslab.text.v1`) is migrated into IndexedDB once on first load.

/// The find/replace dialog state persisted so reopening restores the last
/// session. Defined outside the per-target `imp` modules; each only decides
/// whether it can be stored (localStorage on wasm, nowhere on native).
#[derive(Clone)]
pub struct SavedFindState {
    pub find: String,
    pub replace: String,
    pub regex: bool,
    pub case_insensitive: bool,
    pub replace_focused: bool,
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::cell::RefCell;

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{IdbDatabase, IdbFactory, IdbRequest, IdbTransactionMode, Storage, Window};

    const DB_NAME: &str = "zapslab";
    const DB_VERSION: u32 = 1;
    const STORE: &str = "documents";
    const TEXT_KEY: &str = "text.v1";
    /// Pre-IndexedDB text location, migrated once and then removed.
    const LEGACY_TEXT_KEY: &str = "zapslab.text.v1";
    /// Most-recently-used transform names, newest first. Versioned like the
    /// text key: an older/malformed value is ignored rather than migrated.
    const RECENT_KEY: &str = "zapslab.recent.v1";
    /// Recent find queries, newest first (the view caps the list at 20).
    const FINDS_KEY: &str = "zapslab.finds.v1";
    /// The last find/replace dialog state, restored when the dialog reopens.
    const FIND_STATE_KEY: &str = "zapslab.find-state.v1";

    thread_local! {
        /// The opened database, reused across reads and writes.
        static DATABASE: RefCell<Option<IdbDatabase>> = RefCell::new(None);
    }

    fn window() -> Option<Window> {
        web_sys::window()
    }

    fn storage() -> Option<Storage> {
        window()?.local_storage().ok().flatten()
    }

    fn factory() -> Option<IdbFactory> {
        window()?.indexed_db().ok().flatten()
    }

    /// Bridges an IndexedDB request to a future resolving with its result. The
    /// callbacks are one-shot, so `once_into_js` frees them after the first
    /// event instead of leaking them per request.
    fn request_future(request: &IdbRequest) -> JsFuture {
        let promise = js_sys::Promise::new(&mut |resolve, reject| {
            let success = {
                let request = IdbRequest::clone(request);
                Closure::once_into_js(move || {
                    let value = request.result().unwrap_or(JsValue::UNDEFINED);
                    let _ = resolve.call1(&JsValue::UNDEFINED, &value);
                })
            };
            let failure = Closure::once_into_js(move || {
                let _ = reject.call1(
                    &JsValue::UNDEFINED,
                    &JsValue::from_str("indexeddb request failed"),
                );
            });
            request.set_onsuccess(Some(success.unchecked_ref()));
            request.set_onerror(Some(failure.unchecked_ref()));
        });
        JsFuture::from(promise)
    }

    async fn open_database() -> Result<IdbDatabase, ()> {
        let factory = factory().ok_or(())?;
        let open = factory.open_with_u32(DB_NAME, DB_VERSION).map_err(|_| ())?;

        // Create the object store on first run / schema bump.
        let upgrade_request = open.clone();
        let on_upgrade = Closure::<dyn FnMut()>::new(move || {
            if let Ok(value) = upgrade_request.result() {
                if let Ok(db) = value.dyn_into::<IdbDatabase>() {
                    let _ = db.create_object_store(STORE);
                }
            }
        });
        open.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));

        let result = request_future(open.unchecked_ref::<IdbRequest>())
            .await
            .map_err(|_| ())?;
        drop(on_upgrade);
        result.dyn_into::<IdbDatabase>().map_err(|_| ())
    }

    async fn database() -> Result<IdbDatabase, ()> {
        if let Some(database) = DATABASE.with(|slot| slot.borrow().clone()) {
            return Ok(database);
        }
        let database = open_database().await?;
        DATABASE.with(|slot| *slot.borrow_mut() = Some(database.clone()));
        Ok(database)
    }

    async fn read_text() -> Result<Option<String>, ()> {
        let database = database().await?;
        let transaction = database
            .transaction_with_str_and_mode(STORE, IdbTransactionMode::Readonly)
            .map_err(|_| ())?;
        let store = transaction.object_store(STORE).map_err(|_| ())?;
        let request = store.get(&JsValue::from_str(TEXT_KEY)).map_err(|_| ())?;
        let value = request_future(&request).await.map_err(|_| ())?;
        if value.is_undefined() || value.is_null() {
            Ok(None)
        } else {
            value.as_string().map(Some).ok_or(())
        }
    }

    async fn write_text(text: &str) -> Result<(), ()> {
        let database = database().await?;
        let transaction = database
            .transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite)
            .map_err(|_| ())?;
        let store = transaction.object_store(STORE).map_err(|_| ())?;
        let request = store
            .put_with_key(&JsValue::from_str(text), &JsValue::from_str(TEXT_KEY))
            .map_err(|_| ())?;
        request_future(&request).await.map_err(|_| ())?;
        Ok(())
    }

    /// Loads the stored document, migrating the legacy localStorage value on
    /// first run. Returns `None` when nothing is stored or storage is blocked.
    pub async fn load() -> Option<String> {
        if let Ok(Some(text)) = read_text().await {
            return Some(text);
        }

        let legacy = storage().and_then(|storage| storage.get_item(LEGACY_TEXT_KEY).ok().flatten());
        if let Some(text) = &legacy {
            // Only drop the old key once the migration write succeeded, so a
            // failed write never loses the text.
            if write_text(text).await.is_ok() {
                if let Some(storage) = storage() {
                    let _ = storage.remove_item(LEGACY_TEXT_KEY);
                }
            }
        }
        legacy
    }

    /// Persists `text`. Returns `Err(())` when the write fails (quota, storage
    /// blocked, private mode); callers surface this instead of pretending the
    /// save worked.
    pub async fn save(text: &str) -> Result<(), ()> {
        write_text(text).await
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

    pub fn load_recent_finds() -> Vec<String> {
        let Some(raw) = storage().and_then(|storage| storage.get_item(FINDS_KEY).ok().flatten())
        else {
            return Vec::new();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save_recent_finds(finds: &[String]) -> Result<(), ()> {
        let storage = storage().ok_or(())?;
        let encoded = serde_json::to_string(finds).map_err(|_| ())?;
        storage.set_item(FINDS_KEY, &encoded).map_err(|_| ())
    }

    pub fn load_find_state() -> Option<super::SavedFindState> {
        let raw = storage().and_then(|storage| storage.get_item(FIND_STATE_KEY).ok().flatten())?;
        let (find, replace, regex, case_insensitive, replace_focused): (
            String,
            String,
            bool,
            bool,
            bool,
        ) = serde_json::from_str(&raw).ok()?;
        Some(super::SavedFindState {
            find,
            replace,
            regex,
            case_insensitive,
            replace_focused,
        })
    }

    pub fn save_find_state(state: &super::SavedFindState) -> Result<(), ()> {
        let storage = storage().ok_or(())?;
        let encoded = serde_json::to_string(&(
            state.find.as_str(),
            state.replace.as_str(),
            state.regex,
            state.case_insensitive,
            state.replace_focused,
        ))
        .map_err(|_| ())?;
        storage.set_item(FIND_STATE_KEY, &encoded).map_err(|_| ())
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub async fn load() -> Option<String> {
        None
    }

    pub async fn save(_text: &str) -> Result<(), ()> {
        Ok(())
    }

    pub fn load_recent() -> Vec<String> {
        Vec::new()
    }

    pub fn save_recent(_names: &[String]) -> Result<(), ()> {
        Ok(())
    }

    pub fn load_recent_finds() -> Vec<String> {
        Vec::new()
    }

    pub fn save_recent_finds(_finds: &[String]) -> Result<(), ()> {
        Ok(())
    }

    pub fn load_find_state() -> Option<super::SavedFindState> {
        None
    }

    pub fn save_find_state(_state: &super::SavedFindState) -> Result<(), ()> {
        Ok(())
    }
}

pub use imp::{
    load, load_find_state, load_recent, load_recent_finds, save, save_find_state, save_recent,
    save_recent_finds,
};
