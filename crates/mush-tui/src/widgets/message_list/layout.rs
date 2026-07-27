//! document layout for the message list
//!
//! maps every message onto a range of absolute document rows so a
//! [`ScrollPosition`] can be resolved to a row, and a row back to a
//! position, outside the render pass. heights come from the shared height
//! cache, so the layout the renderer works from and the layout a keypress
//! works from agree by construction.

use std::ops::Range;

use super::{clamp_usize_to_u16, estimate_message_height, estimate_trailing_height};
use crate::app::{App, ScrollPosition};

/// document rows occupied by one message
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MessageLayoutEntry {
    pub(crate) msg_idx: usize,
    pub(crate) start: usize,
    pub(crate) height: usize,
}

impl MessageLayoutEntry {
    pub(crate) fn end(self) -> usize {
        self.start + self.height
    }
}

/// the whole conversation laid out as document rows: every non-queued
/// message in order, then the trailing block (in-flight stream, tool args
/// preview, queued steering messages)
#[derive(Debug)]
pub(crate) struct DocumentLayout {
    entries: Vec<MessageLayoutEntry>,
    /// first document row of the trailing block, i.e. the height of the
    /// message list on its own
    trailing_start: usize,
    trailing_height: usize,
}

impl DocumentLayout {
    pub(crate) fn build(app: &App, width: u16) -> Self {
        let mut entries = Vec::with_capacity(app.messages.len());
        let mut trailing_start = 0usize;

        for (msg_idx, msg) in app.messages.iter().enumerate() {
            if msg.queued {
                continue;
            }
            let height = estimate_message_height(msg, msg_idx, width, &app.render_state);
            entries.push(MessageLayoutEntry {
                msg_idx,
                start: trailing_start,
                height,
            });
            trailing_start += height;
        }

        Self {
            entries,
            trailing_start,
            trailing_height: estimate_trailing_height(app, width, &app.render_state),
        }
    }

    pub(crate) fn entries(&self) -> &[MessageLayoutEntry] {
        &self.entries
    }

    /// document row where the trailing block begins
    pub(crate) fn trailing_start(&self) -> usize {
        self.trailing_start
    }

    /// highest top row that still fills a `vis`-row viewport
    fn max_top(&self, vis: u16) -> usize {
        (self.trailing_start + self.trailing_height).saturating_sub(vis as usize)
    }

    pub(crate) fn entry(&self, msg_idx: usize) -> Option<MessageLayoutEntry> {
        let idx = self
            .entries
            .binary_search_by_key(&msg_idx, |entry| entry.msg_idx)
            .ok()?;
        Some(self.entries[idx])
    }

    /// document row the viewport's top edge sits on. clamped to the last
    /// full screen of content, without touching the stored position, so a
    /// viewport that grows and shrinks again lands back where it started
    pub(crate) fn top_row(&self, position: ScrollPosition, vis: u16) -> usize {
        let max_top = self.max_top(vis);
        let row = match position {
            ScrollPosition::Bottom => max_top,
            ScrollPosition::Message { msg, line } => self
                .entry(msg)
                .map_or(max_top, |entry| anchor_row(entry.start, entry.height, line)),
            ScrollPosition::Trailing { line } => self.trailing_start + line as usize,
        };
        row.min(max_top)
    }

    /// the position that pins the viewport's top edge to `row`
    pub(crate) fn position_at(&self, row: usize, vis: u16) -> ScrollPosition {
        if row >= self.max_top(vis) {
            return ScrollPosition::Bottom;
        }
        if row >= self.trailing_start {
            return ScrollPosition::Trailing {
                line: clamp_usize_to_u16(row - self.trailing_start),
            };
        }
        match self.entries[..self.entries.partition_point(|entry| entry.start <= row)].last() {
            Some(entry) => ScrollPosition::Message {
                msg: entry.msg_idx,
                line: clamp_usize_to_u16(row - entry.start),
            },
            None => ScrollPosition::Bottom,
        }
    }

    /// entries overlapping the document rows `[render_start, render_end)`
    pub(crate) fn visible_entry_range(
        &self,
        render_start: usize,
        render_end: usize,
    ) -> Range<usize> {
        if render_start >= render_end {
            return 0..0;
        }
        let start = self
            .entries
            .partition_point(|entry| entry.end() <= render_start);
        let end = start + self.entries[start..].partition_point(|entry| entry.start < render_end);
        start..end
    }
}

/// document row of a message anchor, keeping the row inside the message it
/// names. a message that reflowed or collapsed shorter than the stored line
/// offset would otherwise drag the anchor into its neighbours
pub(crate) fn anchor_row(msg_start: usize, msg_height: usize, line: u16) -> usize {
    msg_start + (line as usize).min(msg_height.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// three 10-row messages then a 5-row trailing block
    fn layout() -> DocumentLayout {
        DocumentLayout {
            entries: (0..3)
                .map(|msg_idx| MessageLayoutEntry {
                    msg_idx,
                    start: msg_idx * 10,
                    height: 10,
                })
                .collect(),
            trailing_start: 30,
            trailing_height: 5,
        }
    }

    #[test]
    fn rows_round_trip_through_positions() {
        let layout = layout();
        for row in 0..20 {
            let position = layout.position_at(row, 10);
            assert_eq!(
                row,
                layout.top_row(position, 10),
                "row {row} did not survive the round trip as {position:?}"
            );
        }
    }

    #[test]
    fn rows_map_to_the_message_that_owns_them() {
        let layout = layout();
        assert_eq!(
            ScrollPosition::Message { msg: 1, line: 3 },
            layout.position_at(13, 10)
        );
    }

    #[test]
    fn rows_past_the_last_message_anchor_to_the_trailing_block() {
        let layout = layout();
        // total 35 rows, so a 2 row viewport can still top out at row 32
        assert_eq!(
            ScrollPosition::Trailing { line: 2 },
            layout.position_at(32, 2)
        );
    }

    #[test]
    fn the_last_screen_of_content_is_the_follow_position() {
        let layout = layout();
        // total 35 rows in a 10 row viewport: row 25 is the last top row
        assert_eq!(ScrollPosition::Bottom, layout.position_at(25, 10));
        assert_eq!(25, layout.top_row(ScrollPosition::Bottom, 10));
    }

    #[test]
    fn an_anchor_cannot_outlive_its_message() {
        let layout = layout();
        // message 1 is 10 rows, so a line offset past its end clamps to
        // its last row instead of drifting into message 2
        assert_eq!(
            19,
            layout.top_row(ScrollPosition::Message { msg: 1, line: 40 }, 10)
        );
    }

    #[test]
    fn a_missing_message_falls_back_to_following_the_bottom() {
        let layout = layout();
        assert_eq!(
            layout.top_row(ScrollPosition::Bottom, 10),
            layout.top_row(ScrollPosition::Message { msg: 99, line: 0 }, 10)
        );
    }
}
