//! Palette commands that open UI instead of rewriting text.
//!
//! The transform palette mixes text transformations (a pure
//! `fn(&str) -> TransformResult`) with a few commands that open a dialog. These
//! commands live here, separately from [`crate::transforms`], because they have
//! no `apply` function; the view matches them alongside transforms using the
//! same name/tags/description scoring.

/// The action a command performs when the palette runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandId {
    /// Open find with the query field focused.
    OpenFind,
    /// Open find/replace with the replacement field focused.
    OpenFindReplace,
}

/// A palette entry that opens UI instead of transforming text.
#[derive(Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
    pub tags: &'static str,
    pub id: CommandId,
}

/// Commands offered alongside the transform registry. Names are matched first,
/// so keep them short and obvious; put synonyms in `tags`.
pub static COMMANDS: &[Command] = &[
    Command {
        name: "Find",
        description: "Search the document (Ctrl+F)",
        tags: "find search locate",
        id: CommandId::OpenFind,
    },
    Command {
        name: "Find and Replace",
        description: "Find and replace with regex support (Ctrl+F)",
        tags: "find replace search regex",
        id: CommandId::OpenFindReplace,
    },
];
