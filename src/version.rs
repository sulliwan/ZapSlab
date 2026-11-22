//! App identity and "a newer build is deployed" detection.
//!
//! The web build stamps every deploy with a content-hash build id (see
//! `scripts/build_web.sh`), publishes it in `/version.json`, and embeds it in
//! the served `index.html`. The page compares the deployed id against the id it
//! booted with whenever it regains focus or visibility, then forwards a mismatch
//! here as a `window` `message`. The view surfaces that as a "new version"
//! button in the palette header and, on confirmation, reloads.
//!
//! The listener is wasm-only; the non-wasm stub keeps the public signatures
//! uniform so the view compiles on both targets (the same split `storage.rs`
//! uses). Detection deliberately keys off the build id rather than
//! [`APP_VERSION`], so re-releasing the same semantic version with a different
//! binary is still detected.

/// Branded application name shown above the palette. Distinct from
/// `CARGO_PKG_NAME` (the lowercase crate name `zapslab`).
pub const APP_NAME: &str = "ZapSlab";

/// Semantic version, bumped with `make bump_version major|minor|revision`.
/// Display-only: deploy detection uses the build id.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A newer deployed build, reported by the page's version check.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    /// Semantic version of the deployed build, when the page reported one.
    pub version: Option<String>,
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;
    use wasm_bindgen::closure::Closure;
    use web_sys::MessageEvent;

    use super::UpdateInfo;

    const UPDATE_MESSAGE: &str = "zapslab-update-available";

    /// Keeps the JS `message` listener alive for as long as the view holds it.
    pub struct UpdateListener {
        _closure: Closure<dyn FnMut(MessageEvent)>,
    }

    /// Listens for `{ type: "zapslab-update-available" }` messages posted by the
    /// page's inline version check and forwards each to `sender`.
    pub fn listen(sender: async_channel::Sender<UpdateInfo>) -> UpdateListener {
        let closure = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            let field = |name: &str| {
                js_sys::Reflect::get(&data, &JsValue::from_str(name))
                    .ok()
                    .and_then(|value| value.as_string())
            };
            if field("type").as_deref() != Some(UPDATE_MESSAGE) {
                return;
            }
            // Unbounded and never closed while the app lives, so a duplicate
            // message from a later focus check is simply queued again.
            let _ = sender.try_send(UpdateInfo {
                version: field("version"),
            });
        });
        if let Some(window) = web_sys::window() {
            let _ = window.add_event_listener_with_callback(
                "message",
                closure.as_ref().unchecked_ref::<js_sys::Function>(),
            );
        }
        UpdateListener { _closure: closure }
    }

    /// Reloads the page so the service worker serves the newly deployed build.
    pub fn reload() {
        if let Some(window) = web_sys::window() {
            let _ = window.location().reload();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::UpdateInfo;

    /// No-op stand-in; updates are only offered on the web target.
    pub struct UpdateListener;

    pub fn listen(_sender: async_channel::Sender<UpdateInfo>) -> UpdateListener {
        UpdateListener
    }

    /// No-op stand-in; updates are only offered on the web target.
    pub fn reload() {}
}

pub use imp::{UpdateListener, listen, reload};
