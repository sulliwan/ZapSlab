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
mod punycode;
pub(crate) mod random;
mod stats;
mod text;
mod unicode;

use std::{collections::HashMap, sync::LazyLock};

use zapslab_document::encoding::Encoding;

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
    all.extend_from_slice(punycode::TRANSFORMS);
    all.extend_from_slice(numbers::TRANSFORMS);
    all.extend_from_slice(hashing::TRANSFORMS);
    all.extend_from_slice(json::TRANSFORMS);
    all.extend_from_slice(format::TRANSFORMS);
    all.extend_from_slice(lines::TRANSFORMS);
    all.extend_from_slice(stats::TRANSFORMS);
    all.extend_from_slice(text::TRANSFORMS);
    all.extend_from_slice(unicode::TRANSFORMS);
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

/// Applies a transform that interprets decoded bytes as text, using the hex
/// pane's current `encoding`. Returns `None` for ordinary transforms, which are
/// applied with [`Transform::apply`].
///
/// Base64 Decode is currently the only byte-interpreting entry: decoding under
/// the hex pane's encoding is what lets arbitrary binary decode as Latin-1
/// instead of failing on "not valid UTF-8".
pub(crate) fn apply_with_encoding(
    transform: &Transform,
    input: &str,
    encoding: Encoding,
) -> Option<TransformResult> {
    if transform.name == encoding::BASE64_DECODE_NAME {
        Some(encoding::base64_decode_with_encoding(input, encoding))
    } else {
        None
    }
}

/// Transforms that can take noticeably long on a large input (sorts, parsers,
/// formatters, shuffles).
///
/// Kept as a name list rather than a per-entry field so adding an ordinary
/// transform does not require touching every other entry. The
/// `expensive_names_are_registered` test fails if one of these names is renamed
/// or removed, so the list cannot silently go stale.
const EXPENSIVE_TRANSFORMS: &[&str] = &[
    "Sort lines",
    "Natural Sort Lines",
    "Shuffle Lines",
    "Remove Duplicate Lines",
    "Format JSON",
    "Minify JSON",
    "Sort JSON",
    "JSON to CSV",
    "CSV to JSON",
    "JSON to YAML",
    "YAML to JSON",
    "JSON to Query String",
    "Query String to JSON",
    "JWT Decode",
    "Format XML",
    "Minify XML",
    "Format SQL",
    "Minify SQL",
    "Eval Javascript",
];

/// Whether a transform can take noticeably long on a large input. The view
/// confirms before running one of these against a big range so the UI does not
/// appear to hang.
pub fn is_expensive(transform: &Transform) -> bool {
    EXPENSIVE_TRANSFORMS.contains(&transform.name)
}

/// Relevance of a palette entry (a transform or a command) for a lowercased
/// `query`, or `None` if it does not match. Name matches always outrank
/// tag/description matches. Shared with [`crate::commands`] so commands rank
/// against transforms using the same rules.
pub(crate) fn match_score_parts(
    name: &str,
    tags: &str,
    description: &str,
    query: &str,
) -> Option<u32> {
    let name = name.to_lowercase();
    let tags = tags.to_lowercase();
    if name == query {
        Some(100)
    } else if name.starts_with(query) {
        Some(80)
    } else if name.split_whitespace().any(|word| word.starts_with(query)) {
        Some(60)
    } else if name.contains(query) {
        Some(50)
    } else if tags.split(',').any(|tag| tag.trim().starts_with(query)) {
        Some(30)
    } else if tags.contains(query) {
        Some(20)
    } else if description.to_lowercase().contains(query) {
        Some(10)
    } else {
        None
    }
}

/// Palette ordering for `query`: best match first.
///
/// A command's own name outranks a substring that only appears in another
/// entry's description, so "bas" lists "Base64 …" ahead of "Deburr" (whose
/// description says "basic"). Recently used transforms are promoted: with an
/// empty query they lead in recency order, and with a query they only break
/// relevance ties.
pub fn search(query: &str, recent: &[String]) -> Vec<usize> {
    let query = query.trim().to_lowercase();

    if query.is_empty() {
        let mut ordered = Vec::with_capacity(TRANSFORMS.len());
        let mut included = vec![false; TRANSFORMS.len()];
        for name in recent {
            if let Some(index) = TRANSFORMS
                .iter()
                .position(|transform| transform.name == name.as_str())
            {
                if !included[index] {
                    included[index] = true;
                    ordered.push(index);
                }
            }
        }
        for (index, _) in TRANSFORMS.iter().enumerate() {
            if !included[index] {
                ordered.push(index);
            }
        }
        return ordered;
    }

    rank_matches(&query, recent, &TRANSFORMS[..], |index, transform| {
        (transform.name, transform.tags, transform.description, index)
    })
}

/// Scores `entries` against a lowercased `query` and returns the positions of
/// the matches ordered best-first: higher score, then lower recency rank, then
/// the caller's `order` tiebreak.
///
/// `describe` returns `(name, tags, description, order)` for an entry. Both the
/// transform-only [`search`] and the view's combined transform+command palette
/// use this, so their ranking rules cannot drift.
pub(crate) fn rank_matches<T>(
    query: &str,
    recent: &[String],
    entries: &[T],
    describe: impl Fn(usize, &T) -> (&'static str, &'static str, &'static str, usize),
) -> Vec<usize> {
    let recency = recency_ranks(recent);
    let mut scored: Vec<(u32, usize, usize, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| {
            let (name, tags, description, order) = describe(position, entry);
            let score = match_score_parts(name, tags, description, query)?;
            let recent_rank = recency.get(name).copied().unwrap_or(usize::MAX);
            Some((score, recent_rank, order, position))
        })
        .collect();
    scored.sort_by(|a, b| compare_ranked(&(a.0, a.1, a.2), &(b.0, b.1, b.2)));
    scored
        .into_iter()
        .map(|(_, _, _, position)| position)
        .collect()
}

/// Recency rank per transform name (0 = newest); duplicate names keep the
/// first occurrence.
fn recency_ranks(recent: &[String]) -> HashMap<&str, usize> {
    let mut recency = HashMap::new();
    for (position, name) in recent.iter().enumerate() {
        recency.entry(name.as_str()).or_insert(position);
    }
    recency
}

/// Ordering for palette candidates: higher score first, then lower recency
/// rank, then declaration order.
fn compare_ranked(a: &(u32, usize, usize), b: &(u32, usize, usize)) -> std::cmp::Ordering {
    b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position_of(order: &[usize], name: &str) -> usize {
        order
            .iter()
            .position(|&index| TRANSFORMS[index].name == name)
            .unwrap_or_else(|| panic!("{name} missing from search results"))
    }

    #[test]
    fn empty_query_keeps_registry_order() {
        let order = search("", &[]);
        assert_eq!(order, (0..TRANSFORMS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn name_prefix_beats_description_substring() {
        // Regression: "bas" must find the Base64 commands, not Deburr (whose
        // description contains "basic").
        let order = search("bas", &[]);
        assert!(position_of(&order, "Base64 Encode") < position_of(&order, "Deburr"));
        assert!(position_of(&order, "Base64 Decode") < position_of(&order, "Deburr"));
    }

    #[test]
    fn exact_name_ranks_first() {
        assert_eq!(TRANSFORMS[search("rot13", &[])[0]].name, "Rot13");
    }

    #[test]
    fn recent_transforms_lead_the_empty_query() {
        let recent = vec!["Rot13".to_string(), "MD5 Checksum".to_string()];
        let order = search("", &recent);
        assert_eq!(TRANSFORMS[order[0]].name, "Rot13");
        assert_eq!(TRANSFORMS[order[1]].name, "MD5 Checksum");

        // Every transform still appears exactly once.
        let mut indices = order.clone();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(indices.len(), TRANSFORMS.len());
    }

    #[test]
    fn recency_breaks_relevance_ties() {
        let recent = vec!["Minify JSON".to_string()];
        let order = search("json", &recent);
        assert!(position_of(&order, "Minify JSON") < position_of(&order, "Format JSON"));
    }

    #[test]
    fn flags_expensive_transforms() {
        let find = |name: &str| {
            TRANSFORMS
                .iter()
                .find(|transform| transform.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        assert!(is_expensive(find("Sort lines")));
        assert!(is_expensive(find("Format JSON")));
        assert!(is_expensive(find("Format SQL")));
        assert!(!is_expensive(find("Rot13")));
        assert!(!is_expensive(find("MD5 Checksum")));
        assert!(!is_expensive(find("Count Characters")));
    }

    #[test]
    fn expensive_names_are_registered() {
        for name in EXPENSIVE_TRANSFORMS {
            assert!(
                TRANSFORMS.iter().any(|transform| transform.name == *name),
                "expensive transform {name:?} is not in the registry"
            );
        }
    }

    #[test]
    fn base64_decode_is_encoding_aware() {
        let find = |name: &str| {
            TRANSFORMS
                .iter()
                .find(|transform| transform.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        // The registered name the view matches on must exist.
        let base = find(encoding::BASE64_DECODE_NAME);
        assert!(apply_with_encoding(base, "/w==", Encoding::Latin1).is_some());
        // Ordinary transforms keep using their plain `apply`.
        assert!(apply_with_encoding(find("Rot13"), "hello", Encoding::Utf8).is_none());
    }

    #[test]
    fn unknown_recent_names_are_ignored() {
        let recent = vec!["Not A Transform".to_string()];
        assert_eq!(
            search("", &recent),
            (0..TRANSFORMS.len()).collect::<Vec<_>>()
        );
    }
}
