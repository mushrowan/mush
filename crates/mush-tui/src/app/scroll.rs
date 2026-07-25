//! scroll position and viewport maths
//!
//! `scroll_offset` counts lines above the bottom of the current content.
//! the renderer keeps the viewport still while content grows by adding a
//! compensation on top, so any keypress has to absorb that compensation
//! first (see [`App::absorb_scroll_compensation`]).

use super::*;

impl App {
    /// jump to bottom of conversation and clear unread indicator
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.navigation.has_unread = false;
    }

    /// fold the renderer's pinning compensation into `scroll_offset` so
    /// it means "lines above the bottom of the current content" again,
    /// and re-pin the baseline to the last rendered frame.
    ///
    /// while the user sits scrolled up the renderer holds the viewport
    /// still by adding `total_lines - baseline` on top of the offset
    /// rather than touching `scroll_offset` itself. a scroll keypress
    /// has to move relative to what is on screen, so absorb that
    /// compensation first. without this, content that arrived while
    /// scrolled up is invisible to the offset arithmetic and the first
    /// press that drives the offset to 0 snaps to the real bottom
    fn absorb_scroll_compensation(&mut self) {
        let compensation = self.render_state.scroll_compensation.get();
        if self.scroll_offset == 0 || compensation == 0 {
            return;
        }
        let absorbed = (self.scroll_offset as isize + compensation).clamp(0, u16::MAX as isize);
        self.scroll_offset = absorbed as u16;
        self.render_state
            .scroll_baseline
            .set(self.render_state.prev_content_lines.get());
        self.render_state
            .scroll_baseline_vis
            .set(self.render_state.visible_area_height.get());
        self.render_state.scroll_compensation.set(0);
    }

    /// scroll `lines` towards older content
    pub fn scroll_up(&mut self, lines: u16) {
        self.absorb_scroll_compensation();
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    /// scroll `lines` towards newer content, clearing the unread
    /// indicator once the bottom is reached
    pub fn scroll_down(&mut self, lines: u16) {
        self.absorb_scroll_compensation();
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        if self.scroll_offset == 0 {
            self.navigation.has_unread = false;
        }
    }

    /// jump to the top of the conversation. used after a manual /compact
    /// so the freshly generated summary (pushed at index 0) is the first
    /// thing the user sees, instead of leaving them at the bottom looking
    /// at the same kept messages they had before. setting `u16::MAX` is
    /// safe because the renderer clamps `scroll_offset` to
    /// `total_lines - visible_lines` at draw time
    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = u16::MAX;
    }

    /// whether the message at `msg_idx` overlaps the current viewport based
    /// on cached heights from the last render. returns true when no render
    /// has happened yet (`visible_area_height == 0`) so callers do not block
    /// on missing data. used to gate scroll-mode selection movement so we
    /// never select a message that is off-screen
    pub fn is_message_visible(&self, msg_idx: usize) -> bool {
        if msg_idx >= self.messages.len() {
            return false;
        }
        let visible = self.render_state.visible_area_height.get() as usize;
        if visible == 0 {
            return true;
        }
        let cache = self.render_state.height_cache.borrow();
        let height_of = |i: usize| -> usize {
            if self.messages.get(i).is_none_or(|m| m.queued) {
                return 0;
            }
            cache
                .get(i)
                .and_then(|e| e.as_ref())
                .map_or(0, |e| e.height)
        };
        let msg_start: usize = (0..msg_idx).map(height_of).sum();
        let msg_height = height_of(msg_idx);
        let total: usize = (0..self.messages.len()).map(height_of).sum();
        drop(cache);

        let scroll_offset = (self.scroll_offset as usize).min(total.saturating_sub(visible));
        let top_of_view = total.saturating_sub(scroll_offset + visible);
        let bottom_of_view = total.saturating_sub(scroll_offset);

        msg_start < bottom_of_view && msg_start + msg_height > top_of_view
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
