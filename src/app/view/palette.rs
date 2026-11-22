//! Methods moved out of `src/app.rs` during the view decomposition.
//!
//! This module is a child of `app`, so the `impl Scratchpad` block below can
//! use the view's private fields and helpers.

use super::super::*;

impl Scratchpad {
    /// Palette entries matching the query, best match first. Recently used
    /// transforms lead when the query is empty.
    pub(crate) fn palette_matches(&self) -> Vec<PaletteEntry> {
        let query = self
            .palette
            .as_ref()
            .map(|palette| palette.query.as_str())
            .unwrap_or("");

        // Empty query: commands lead, then the transform order with recents
        // promoted (matching `transforms::search`).
        if query.trim().is_empty() {
            let mut ordered: Vec<PaletteEntry> = (0..commands::COMMANDS.len())
                .map(PaletteEntry::Command)
                .collect();
            ordered.extend(
                transforms::search("", &self.recent_transforms)
                    .into_iter()
                    .map(PaletteEntry::Transform),
            );
            return ordered;
        }

        let query = query.trim().to_lowercase();
        let recency = transforms::recency_ranks(&self.recent_transforms);

        let mut scored: Vec<(u32, usize, usize, PaletteEntry)> = Vec::new();
        for (index, transform) in transforms::TRANSFORMS.iter().enumerate() {
            if let Some(score) = transforms::match_score_parts(
                transform.name,
                transform.tags,
                transform.description,
                &query,
            ) {
                let rank = recency.get(transform.name).copied().unwrap_or(usize::MAX);
                scored.push((score, rank, index, PaletteEntry::Transform(index)));
            }
        }
        for (index, command) in commands::COMMANDS.iter().enumerate() {
            if let Some(score) = transforms::match_score_parts(
                command.name,
                command.tags,
                command.description,
                &query,
            ) {
                scored.push((score, usize::MAX, index, PaletteEntry::Command(index)));
            }
        }
        scored.sort_by(|a, b| transforms::compare_ranked(&(a.0, a.1, a.2), &(b.0, b.1, b.2)));
        scored.into_iter().map(|(_, _, _, entry)| entry).collect()
    }

    pub(crate) fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let (sel_start, sel_end) = self.document.selection_range();
        let has_selection = sel_start != sel_end;
        let len = matches.len() as isize;
        let selected = {
            let Some(palette) = &mut self.palette else {
                return;
            };
            let mut current = palette.selected.min(matches.len() - 1) as isize;
            // Skip entries that cannot run (selection-only with no selection).
            // Bounded by `len` so an all-unavailable list never spins.
            for _ in 0..len {
                current = (current + delta).clamp(0, len - 1);
                if palette_entry_available(matches[current as usize], has_selection) {
                    break;
                }
            }
            palette.selected = current as usize;
            palette.selected
        };
        // Keep the highlighted entry visible while arrowing through the list.
        self.palette_scroll.scroll_to_item(selected);
        cx.notify();
    }

    pub(crate) fn apply_selected_palette_entry(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let selected = self.palette.as_ref().map_or(0, |palette| palette.selected);
        let entry = matches[selected.min(matches.len() - 1)];

        // Commands open UI; they close the palette and never touch the document.
        if let PaletteEntry::Command(index) = entry {
            let command = commands::COMMANDS[index];
            self.palette = None;
            window.notify_text_input_changed(&self.focus_handle);
            match command.id {
                commands::CommandId::OpenFind => self.open_find(window, cx),
                commands::CommandId::OpenFindReplace => self.open_find_replace(window, cx),
            }
            return;
        }

        let PaletteEntry::Transform(index) = entry else {
            return;
        };
        let transform = transforms::TRANSFORMS[index];

        let (sel_start, sel_end) = self.document.selection_range();
        let has_selection = sel_start != sel_end;
        if !transforms::is_available(&transform, has_selection) {
            self.show_hint("Select text to transform", cx);
            return;
        }

        self.record_recent_transform(transform.name);

        // Resolve the read/write range from the transform's scope. Boop's
        // `state.text` is the selection when one exists, else the whole buffer.
        let (start, end) = match transform.scope {
            transforms::Scope::Selection => (sel_start, sel_end),
            transforms::Scope::FullText => (0, self.document.len()),
            transforms::Scope::SelectionOrFullText => {
                if has_selection {
                    (sel_start, sel_end)
                } else {
                    (0, self.document.len())
                }
            }
        };

        // A heavy transform over a big range can block the UI for a while;
        // confirm first instead of appearing to hang.
        if transforms::is_expensive(&transform)
            && end.saturating_sub(start) > TRANSFORM_CONFIRM_BYTES
        {
            self.palette = None;
            self.pending_transform = Some(PendingTransform {
                index,
                start,
                end,
                keep_selection: has_selection,
            });
            cx.notify();
            return;
        }

        self.run_transform(index, start, end, has_selection, window, cx);
    }

    /// Runs a transform over `start..end` (already resolved from its scope).
    pub(crate) fn run_transform(
        &mut self,
        index: usize,
        start: usize,
        end: usize,
        keep_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let transform = transforms::TRANSFORMS[index];
        let result = (transform.apply)(&self.document.text()[start..end]);
        self.palette = None;
        match result {
            transforms::TransformResult::Replace(text) => {
                self.replace_range_keeping_selection(start, end, &text, keep_selection, window, cx);
            }
            transforms::TransformResult::ReplaceAndReport { text, message } => {
                self.replace_range_keeping_selection(start, end, &text, keep_selection, window, cx);
                self.show_hint(message, cx);
            }
            transforms::TransformResult::Report(message) => {
                self.show_hint(message, cx);
            }
            transforms::TransformResult::Error(message) => {
                self.show_hint(message, cx);
            }
        }
        cx.notify();
    }

    /// Confirms the pending expensive transform (Enter or the Run button).
    pub(crate) fn confirm_pending_transform(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.pending_transform.take() {
            self.run_transform(
                pending.index,
                pending.start,
                pending.end,
                pending.keep_selection,
                window,
                cx,
            );
        }
    }

    /// Dismisses the pending transform without running it.
    pub(crate) fn cancel_pending_transform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_transform = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Promotes `name` to the front of the recently-used list and persists it.
    /// Persistence is a convenience; a failed write is intentionally ignored so
    /// it cannot disrupt the edit.
    pub(crate) fn record_recent_transform(&mut self, name: &'static str) {
        self.recent_transforms.retain(|recent| recent != name);
        self.recent_transforms.insert(0, name.to_string());
        self.recent_transforms.truncate(RECENT_TRANSFORMS_MAX);
        let _ = storage::save_recent(&self.recent_transforms);
    }

    #[allow(dead_code)] // used by the native key path
    pub(crate) fn push_palette_query(&mut self, text: &str) {
        if let Some(palette) = &mut self.palette {
            palette.query.push_str(text);
            palette.selected = 0;
        }
        self.palette_scroll.set_offset(point(px(0.0), px(0.0)));
    }
}
