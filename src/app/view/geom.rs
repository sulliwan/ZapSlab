//! Pure scrollbar geometry shared by the editor, the hex pane, and their drag
//! handlers.
//!
//! gpui draws no scrollbars for overflow containers, so the view paints and
//! drags its own. `ScrollbarGeometry` centralizes the thumb size, travel, and
//! offset math so a painted thumb and its hit area cannot drift apart, and so
//! the three custom scrollbars stay consistent.

use super::super::MIN_THUMB_WIDTH;

/// Thumb layout for one axis of a custom scrollbar.
#[derive(Clone, Copy)]
pub(crate) struct ScrollbarGeometry {
    /// Scrollable extent beyond the viewport.
    pub(crate) overflow: f32,
    /// Thumb size along the track, in pixels.
    pub(crate) thumb: f32,
    /// Distance the thumb can travel (track minus thumb).
    pub(crate) travel: f32,
}

impl ScrollbarGeometry {
    /// `content` and `viewport` are sizes along the scroll axis (height for a
    /// vertical bar, width for a horizontal one); `track` is the usable track
    /// length. Returns `None` when there is nothing to scroll.
    pub(crate) fn new(content: f32, viewport: f32, track: f32) -> Option<Self> {
        if !content.is_finite() || !viewport.is_finite() || track <= 0.0 {
            return None;
        }
        let overflow = (content - viewport).max(0.0);
        if overflow <= 0.0 {
            return None;
        }
        let min_thumb = MIN_THUMB_WIDTH.min(track);
        let thumb = (track * viewport / content).clamp(min_thumb, track);
        let travel = (track - thumb).max(0.0);
        if travel <= 0.0 {
            return None;
        }
        Some(Self {
            overflow,
            thumb,
            travel,
        })
    }

    /// Fraction scrolled (0..=1) for gpui's signed scroll offset (which is
    /// `-scroll_top`).
    pub(crate) fn fraction_from_offset(&self, offset: f32) -> f32 {
        (-offset / self.overflow).clamp(0.0, 1.0)
    }

    /// Thumb position along the track for `fraction`, offset by `margin`.
    pub(crate) fn thumb_start(&self, margin: f32, fraction: f32) -> f32 {
        margin + fraction * self.travel
    }

    /// Signed gpui scroll offset (`-scroll_top`) for a pointer on the track.
    pub(crate) fn offset_for_pointer(&self, pointer: f32, grab: f32, margin: f32) -> f32 {
        let fraction = ((pointer - grab - margin) / self.travel).clamp(0.0, 1.0);
        -fraction * self.overflow
    }
}
