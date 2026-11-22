//! Methods moved out of `src/app.rs` during the view decomposition.
//!
//! This module is a child of `app`, so the `impl Scratchpad` block below can
//! use the view's private fields and helpers.

use super::super::*;
use super::geom::ScrollbarGeometry;

impl Scratchpad {
    pub(crate) fn update_scroll_drag(
        &mut self,
        position_x: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.scroll_drag else {
            return false;
        };
        if self.word_wrap {
            self.scroll_drag = None;
            return false;
        }
        // Recompute geometry from the current frame rather than trusting the
        // values captured when the drag started (window may have resized).
        // The editor shrinks when the hex panel is open, so measure its area.
        let viewport_width = self.editor_viewport_width(f32::from(window.viewport_size().width));
        let track_width = (viewport_width - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let Some(geometry) =
            ScrollbarGeometry::new(self.content_width.get(), viewport_width, track_width)
        else {
            self.scroll_drag = None;
            return false;
        };
        let new_x = geometry.offset_for_pointer(f32::from(position_x), drag.grab, SCROLLBAR_MARGIN);
        let offset = self.scroll_handle.offset();
        self.scroll_handle.set_offset(point(px(new_x), offset.y));
        cx.notify();
        true
    }

    pub(crate) fn begin_scroll_drag(
        &mut self,
        position: Point<Pixels>,
        centered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Never let a scrollbar click reach the text handlers, even if the
        // geometry guard below bails out.
        cx.stop_propagation();
        self.dragging = false;

        let viewport_width = self.editor_viewport_width(f32::from(window.viewport_size().width));
        let track_width = (viewport_width - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let Some(geometry) =
            ScrollbarGeometry::new(self.content_width.get(), viewport_width, track_width)
        else {
            return;
        };
        let grab = if centered {
            geometry.thumb / 2.0
        } else {
            let fraction = geometry.fraction_from_offset(f32::from(self.scroll_handle.offset().x));
            let thumb_left = geometry.thumb_start(SCROLLBAR_MARGIN, fraction);
            f32::from(position.x) - thumb_left
        };
        self.scroll_drag = Some(ScrollDrag { grab });
        self.update_scroll_drag(position.x, window, cx);
    }

    pub(crate) fn update_v_scroll_drag(
        &mut self,
        position_y: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.v_scroll_drag else {
            return false;
        };
        // Recompute geometry from the current frame rather than trusting the
        // values captured when the drag started (window may have resized).
        let editor_height = self.editor_height(window);
        let track_height = (editor_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let Some(geometry) =
            ScrollbarGeometry::new(self.content_height.get(), editor_height, track_height)
        else {
            self.v_scroll_drag = None;
            return false;
        };
        let new_y = geometry.offset_for_pointer(position_y, drag.grab, SCROLLBAR_MARGIN);
        let offset = self.scroll_handle.offset();
        self.scroll_handle.set_offset(point(offset.x, px(new_y)));
        cx.notify();
        true
    }

    pub(crate) fn begin_v_scroll_drag(
        &mut self,
        position: Point<Pixels>,
        centered: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        // Never let a scrollbar click reach the text handlers.
        cx.stop_propagation();
        self.dragging = false;

        let editor_height = self.editor_height(window);
        let track_height = (editor_height - 2.0 * SCROLLBAR_MARGIN).max(1.0);
        let Some(geometry) =
            ScrollbarGeometry::new(self.content_height.get(), editor_height, track_height)
        else {
            return;
        };
        let grab = if centered {
            geometry.thumb / 2.0
        } else {
            let fraction = geometry.fraction_from_offset(f32::from(self.scroll_handle.offset().y));
            let thumb_top = geometry.thumb_start(SCROLLBAR_MARGIN, fraction);
            f32::from(position.y) - thumb_top
        };
        self.v_scroll_drag = Some(ScrollDrag { grab });
        self.update_v_scroll_drag(f32::from(position.y), window, cx);
    }
}
