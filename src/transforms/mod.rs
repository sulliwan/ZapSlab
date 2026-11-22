//! Text transformations available from the transform palette.
//!
//! Each entry is a pure `fn(&str) -> TransformResult` plus display metadata.
//! The registry is split into category submodules; [`TRANSFORMS`] flattens
//! them once at first use. Adding a transform is one entry in a submodule's
//! `TRANSFORMS` slice.
//!
//! The `scope` field mirrors Boop's `ScriptExecution` targets:
//! `state.selection`, `state.fullText`, and `state.text` (which is the
//! selection when one exists, otherwise the whole document).

mod case;
mod color;
mod dates;
mod encoding;
mod eval;
mod format;
mod hashing;
mod json;
mod lines;
mod numbers;
mod platform;
pub(crate) mod random;
mod stats;
mod text;

use std::sync::LazyLock;

/// Which text a transform reads and writes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Only the current selection. Unavailable when nothing is selected.
    Selection,
    /// The entire document, regardless of any selection.
    FullText,
    /// The selection when there is one, otherwise the whole document.
    ///
    /// This is Boop's `state.text` behavior and the default for most
    /// transforms, including the "run the shortcut with nothing selected"
    /// case this app exposes.
    SelectionOrFullText,
}

/// Outcome of applying a transform to its input.
pub enum TransformResult {
    /// Replace the target range with this text.
    Replace(String),
    /// Replace the target range and also surface an informational message.
    ReplaceAndReport { text: String, message: String },
    /// Leave the text unchanged and surface an informational message.
    Report(String),
    /// Leave the text unchanged and surface an error message.
    Error(String),
}

impl TransformResult {
    pub fn replace(text: impl Into<String>) -> Self {
        Self::Replace(text.into())
    }

    pub fn replace_and_report(text: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ReplaceAndReport {
            text: text.into(),
            message: message.into(),
        }
    }

    pub fn report(message: impl Into<String>) -> Self {
        Self::Report(message.into())
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }
}

/// A single palette entry.
#[derive(Clone, Copy)]
pub struct Transform {
    pub name: &'static str,
    pub description: &'static str,
    pub tags: &'static str,
    pub scope: Scope,
    pub apply: fn(&str) -> TransformResult,
}

/// Flattened registry, built once from the per-category slices.
pub static TRANSFORMS: LazyLock<Vec<Transform>> = LazyLock::new(|| {
    let mut all = Vec::new();
    all.extend_from_slice(case::TRANSFORMS);
    all.extend_from_slice(encoding::TRANSFORMS);
    all.extend_from_slice(numbers::TRANSFORMS);
    all.extend_from_slice(hashing::TRANSFORMS);
    all.extend_from_slice(json::TRANSFORMS);
    all.extend_from_slice(format::TRANSFORMS);
    all.extend_from_slice(lines::TRANSFORMS);
    all.extend_from_slice(stats::TRANSFORMS);
    all.extend_from_slice(text::TRANSFORMS);
    all.extend_from_slice(dates::TRANSFORMS);
    all.extend_from_slice(platform::TRANSFORMS);
    all.extend_from_slice(color::TRANSFORMS);
    all.extend_from_slice(eval::TRANSFORMS);
    all
});

/// Whether a transform can run given the current selection state.
///
/// `Selection`-scoped transforms have nothing to operate on without a
/// selection; everything else is always applicable.
pub fn is_available(transform: &Transform, has_selection: bool) -> bool {
    !(transform.scope == Scope::Selection && !has_selection)
}
