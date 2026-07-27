//! scroll position and viewport maths
//!
//! the viewport is pinned by a [`ScrollPosition`], which names a document
//! position rather than a distance from the bottom. content arriving below
//! the anchor cannot move the view, and a viewport that changes height just
//! reveals or hides rows below it, so no compensation arithmetic is needed.

use super::*;
use crate::widgets::message_list::DocumentLayout;

impl App {
    /// whether the viewport is pinned above the newest content
    #[must_use]
    pub fn is_scrolled_up(&self) -> bool {
        !self.scroll.is_bottom()
    }

    /// jump to bottom of conversation and clear unread indicator
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = ScrollPosition::Bottom;
        self.navigation.has_unread = false;
    }

    /// jump to the top of the conversation. used after a manual /compact
    /// so the freshly generated summary is the first thing the user sees,
    /// instead of leaving them at the bottom looking at the same kept
    /// messages they had before
    pub fn scroll_to_top(&mut self) {
        self.scroll = match self.document_layout() {
            Some((layout, vis)) => layout.position_at(0, vis),
            None => ScrollPosition::Message { msg: 0, line: 0 },
        };
    }

    /// scroll `lines` towards older content
    pub fn scroll_up(&mut self, lines: u16) {
        self.scroll_by(-(lines as isize));
    }

    /// scroll `lines` towards newer content, clearing the unread
    /// indicator once the bottom is reached
    pub fn scroll_down(&mut self, lines: u16) {
        self.scroll_by(lines as isize);
    }

    /// move the viewport `delta` rows through the document, re-anchoring to
    /// whatever lands on the top edge. moving relative to the row currently
    /// on screen (rather than to the stored anchor) means a keypress always
    /// does what the user can see, even when the anchor was clamped because
    /// the viewport grew past the remaining content
    fn scroll_by(&mut self, delta: isize) {
        let Some((layout, vis)) = self.document_layout() else {
            return;
        };
        let top = layout.top_row(self.scroll, vis);
        let target = top.saturating_add_signed(delta);
        self.scroll = layout.position_at(target, vis);
        if self.scroll.is_bottom() {
            self.navigation.has_unread = false;
        }
        tracing::debug!(
            target: "mush_tui::scroll",
            delta,
            prev_top = top,
            target,
            position = ?self.scroll,
            "scroll moved"
        );
    }

    /// document layout and viewport height as of the last rendered frame.
    /// `None` until a frame has been drawn, since without a width there are
    /// no wrapped-line heights to reason about
    fn document_layout(&self) -> Option<(DocumentLayout, u16)> {
        let area = self.render_state.message_area.get();
        if area.width == 0 || area.height == 0 {
            return None;
        }
        Some((DocumentLayout::build(self, area.width), area.height))
    }

    /// document row at the top of the viewport, from the last rendered
    /// frame's geometry. `None` before the first frame
    #[cfg(test)]
    pub(crate) fn viewport_top(&self) -> Option<usize> {
        let (layout, vis) = self.document_layout()?;
        Some(layout.top_row(self.scroll, vis))
    }

    /// whether the message at `msg_idx` overlaps the current viewport based
    /// on cached heights from the last render. returns true when no render
    /// has happened yet so callers do not block on missing data. used to
    /// gate scroll-mode selection movement so we never select a message
    /// that is off-screen
    pub fn is_message_visible(&self, msg_idx: usize) -> bool {
        if msg_idx >= self.messages.len() {
            return false;
        }
        let Some((layout, vis)) = self.document_layout() else {
            return true;
        };
        let Some(entry) = layout.entry(msg_idx) else {
            return false;
        };
        let top = layout.top_row(self.scroll, vis);
        entry.start < top + vis as usize && entry.end() > top
    }

    /// find which message is displayed at a given screen row
    ///
    /// uses render metadata (message_row_ranges, render_scroll, message_area)
    /// populated by MessageList during the last render pass
    pub fn message_at_screen_row(&self, row: u16) -> Option<usize> {
        let area = self.render_state.message_area.get();
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let screen_line = self.render_state.render_scroll.get() + (row - area.y);
        let ranges = self.render_state.message_row_ranges.borrow();
        ranges
            .iter()
            .find(|r| screen_line >= r.start && screen_line < r.end)
            .map(|r| r.msg_idx)
    }
}
