use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::App;
use crate::ui::app_layout;
use crate::widgets::input_box::InputBox;
use crate::widgets::message_list::{Transcript, TranscriptChunk, transcript_guards};
use crate::widgets::status_bar::StatusBar;
use crate::widgets::top_bar::TopBar;

pub const SCROLLBAR_FADE: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Char,
    Line,
    Block,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub line: usize,
    pub column: usize,
    wanted: usize,
}

#[derive(Debug)]
pub struct Viewport {
    scroll_top: Cell<usize>,
    following: Cell<bool>,
    unread: Cell<bool>,
    total: Cell<usize>,
    height: Cell<usize>,
    moved: Cell<Option<Instant>>,
    cursor: Cell<Cursor>,
    widths: RefCell<Vec<usize>>,
    chunks: RefCell<Vec<TranscriptChunk>>,
    area: Cell<Rect>,
    anchor: Cell<Option<(usize, usize)>>,
    kind: Cell<SelectionKind>,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            scroll_top: Cell::new(0),
            following: Cell::new(true),
            unread: Cell::new(false),
            total: Cell::new(0),
            height: Cell::new(0),
            moved: Cell::new(None),
            cursor: Cell::new(Cursor::default()),
            widths: RefCell::new(Vec::new()),
            chunks: RefCell::new(Vec::new()),
            area: Cell::new(Rect::default()),
            anchor: Cell::new(None),
            kind: Cell::new(SelectionKind::Char),
        }
    }
}

impl Viewport {
    pub(crate) fn place(&self, transcript: &Transcript<'_>, area: Rect) -> Range<usize> {
        self.area.set(area);
        self.remap_chunks(&transcript.chunks);
        let window = self.window(transcript.len(), usize::from(area.height));
        self.widths.replace(
            transcript[window.clone()]
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.chars().count())
                        .sum()
                })
                .collect(),
        );
        self.hold_cursor_on_screen();
        window
    }

    pub(crate) fn remap_chunks(&self, chunks: &[TranscriptChunk]) {
        let previous = self.chunks.replace(chunks.to_vec());
        if previous.is_empty() || self.following.get() {
            return;
        }
        if chunks
            .iter()
            .any(|chunk| previous.iter().all(|seen| seen.id != chunk.id))
        {
            self.unread.set(true);
        }
        let map = |line| remap_line(line, &previous, chunks);
        self.scroll_top.set(map(self.scroll_top.get()));
        let mut cursor = self.cursor.get();
        cursor.line = map(cursor.line);
        self.cursor.set(cursor);
        if let Some((line, column)) = self.anchor.get() {
            self.anchor.set(Some((map(line), column)));
        }
    }

    #[must_use]
    pub(crate) fn message_at_cursor(&self) -> Option<usize> {
        match chunk_at(&self.chunks.borrow(), self.cursor.get().line)?.id {
            crate::widgets::message_list::TranscriptChunkId::Message(index) => Some(index),
            crate::widgets::message_list::TranscriptChunkId::Streaming => None,
        }
    }

    pub fn window(&self, total: usize, height: usize) -> Range<usize> {
        self.total.set(total);
        self.height.set(height);
        let top = if self.following.get() {
            self.max_scroll()
        } else {
            self.scroll_top.get().min(self.max_scroll())
        };
        self.scroll_top.set(top);
        if self.following.get() {
            self.unread.set(false);
        }
        top..total.min(top + height)
    }

    #[must_use]
    pub fn has_unread(&self) -> bool {
        self.unread.get() && !self.following.get()
    }

    #[must_use]
    pub fn shows_the_tail(&self) -> bool {
        self.following.get() || self.window_end() >= self.total.get()
    }

    pub fn pause(&self) {
        self.following.set(false);
        let last = self.window_end().saturating_sub(1);
        self.cursor.set(Cursor {
            line: last,
            column: 0,
            wanted: 0,
        });
    }

    pub fn follow(&self) {
        self.to_bottom();
        self.following.set(true);
    }

    pub fn resume(&self) {
        self.following
            .set(self.scroll_top.get() >= self.max_scroll());
    }

    pub fn scroll_by(&self, lines: isize) {
        let top = self.scroll_top.get().saturating_add_signed(lines);
        self.scroll_to(top);
        self.hold_cursor_on_screen();
    }

    pub fn page_by(&self, lines: isize) {
        self.scroll_by(lines);
        self.move_cursor(0, lines);
    }

    pub fn move_cursor(&self, columns: isize, lines: isize) {
        self.moved.set(Some(Instant::now()));
        let mut cursor = self.cursor.get();
        if columns != 0 {
            cursor.column = cursor.column.saturating_add_signed(columns);
            cursor.wanted = cursor.column;
        }
        if lines != 0 {
            let line = cursor.line.saturating_add_signed(lines);
            cursor.line = line.min(self.total.get().saturating_sub(1));
            self.keep_in_view(cursor.line);
        }
        self.cursor.set(cursor);
        self.clamp_cursor();
    }

    pub fn cursor_to_column(&self, column: usize) {
        let mut cursor = self.cursor.get();
        cursor.column = column;
        cursor.wanted = column;
        self.cursor.set(cursor);
        self.clamp_cursor();
        self.moved.set(Some(Instant::now()));
    }

    pub fn cursor_to(&self, line: usize) {
        let mut cursor = self.cursor.get();
        cursor.line = line.min(self.total.get().saturating_sub(1));
        self.cursor.set(cursor);
        self.keep_in_view(cursor.line);
        self.clamp_cursor();
    }

    pub fn start_selection(&self, kind: SelectionKind) {
        let cursor = self.cursor.get();
        self.anchor.set(Some((cursor.line, cursor.column)));
        self.kind.set(kind);
    }

    pub(crate) fn select(&self, anchor: (usize, usize), head: (usize, usize), kind: SelectionKind) {
        self.anchor.set(Some(anchor));
        self.kind.set(kind);
        self.cursor_to(head.0);
        self.cursor_to_column(head.1);
    }

    pub(crate) fn selection_ends(&self) -> Option<((usize, usize), (usize, usize))> {
        let cursor = self.cursor.get();
        Some((self.anchor.get()?, (cursor.line, cursor.column)))
    }

    pub(crate) fn swap_selection_ends(&self) {
        let cursor = self.cursor.get();
        if let Some((line, column)) = self.anchor.replace(Some((cursor.line, cursor.column))) {
            self.cursor_to(line);
            self.cursor_to_column(column);
        }
    }

    #[must_use]
    pub fn selection_kind(&self) -> SelectionKind {
        self.kind.get()
    }

    pub fn clear_selection(&self) {
        self.anchor.set(None);
    }

    #[must_use]
    pub fn selection(&self) -> Option<std::ops::RangeInclusive<(usize, usize)>> {
        let anchor = self.anchor.get()?;
        let cursor = self.cursor.get();
        let head = (cursor.line, cursor.column);
        Some(anchor.min(head)..=anchor.max(head))
    }

    #[must_use]
    pub fn selected_columns(&self, line: usize) -> Option<Range<usize>> {
        let selection = self.selection()?;
        let (start_line, start_column) = *selection.start();
        let (end_line, end_column) = *selection.end();
        if line < start_line || line > end_line {
            return None;
        }
        match self.kind.get() {
            SelectionKind::Line => return Some(0..self.line_width(line)),
            SelectionKind::Block => {
                let left = start_column.min(end_column);
                let right = start_column.max(end_column) + 1;
                return Some(left..right.min(self.line_width(line)));
            }
            SelectionKind::Char => {}
        }
        let first = if line == start_line { start_column } else { 0 };
        let last = if line == end_line {
            end_column + 1
        } else {
            self.line_width(line)
        };
        Some(first..last)
    }

    #[must_use]
    pub fn area(&self) -> Rect {
        self.area.get()
    }

    #[must_use]
    pub fn cursor(&self) -> Cursor {
        self.cursor.get()
    }

    #[must_use]
    pub fn cursor_row(&self) -> u16 {
        let row = self.cursor.get().line.saturating_sub(self.scroll_top.get());
        u16::try_from(row).unwrap_or(u16::MAX)
    }

    pub fn to_top(&self) {
        self.scroll_to(0);
        self.cursor_to(0);
    }

    pub fn to_bottom(&self) {
        self.scroll_to(self.max_scroll());
        self.cursor_to(self.total.get().saturating_sub(1));
    }

    #[must_use]
    pub fn page(&self) -> usize {
        self.height.get().max(1)
    }

    #[must_use]
    pub fn is_following(&self) -> bool {
        self.following.get()
    }

    #[must_use]
    pub fn thumb(&self, now: Instant) -> Option<Range<usize>> {
        let faded = self
            .moved
            .get()
            .is_none_or(|moved| now.duration_since(moved) > SCROLLBAR_FADE);
        let height = self.height.get();
        if faded || self.total.get() <= height {
            return None;
        }
        let span = (height * height).div_ceil(self.total.get()).max(1);
        let top = (self.scroll_top.get() * height).div_ceil(self.total.get());
        Some(top..(top + span).min(height))
    }

    fn scroll_to(&self, top: usize) {
        let top = top.min(self.max_scroll());
        self.scroll_top.set(top);
        self.following.set(top >= self.max_scroll());
        self.moved.set(Some(Instant::now()));
    }

    fn keep_in_view(&self, line: usize) {
        let top = self.scroll_top.get();
        if line < top {
            self.scroll_to(line);
        } else if line >= top + self.page() {
            self.scroll_to(line + 1 - self.page());
        }
    }

    fn hold_cursor_on_screen(&self) {
        let mut cursor = self.cursor.get();
        let top = self.scroll_top.get();
        cursor.line = cursor.line.clamp(top, self.window_end().saturating_sub(1));
        self.cursor.set(cursor);
        self.clamp_cursor();
    }

    fn clamp_cursor(&self) {
        let mut cursor = self.cursor.get();
        let width = self.line_width(cursor.line);
        cursor.column = cursor.wanted.min(width.saturating_sub(1));
        self.cursor.set(cursor);
    }

    fn line_width(&self, line: usize) -> usize {
        let widths = self.widths.borrow();
        line.checked_sub(self.scroll_top.get())
            .and_then(|row| widths.get(row).copied())
            .unwrap_or(0)
    }

    pub(crate) fn visible_lines(&self) -> (usize, usize) {
        let top = self.scroll_top.get();
        (top, self.window_end().saturating_sub(1).max(top))
    }

    fn window_end(&self) -> usize {
        self.total.get().min(self.scroll_top.get() + self.page())
    }

    fn max_scroll(&self) -> usize {
        self.total.get().saturating_sub(self.page())
    }
}

fn remap_line(line: usize, previous: &[TranscriptChunk], current: &[TranscriptChunk]) -> usize {
    let Some(chunk) = chunk_at(previous, line) else {
        return line;
    };
    let offset = line.saturating_sub(chunk.start);
    current
        .iter()
        .find(|candidate| candidate.id == chunk.id)
        .map_or(line, |candidate| {
            candidate.start + offset.min(candidate.end.saturating_sub(candidate.start + 1))
        })
}

fn chunk_at(chunks: &[TranscriptChunk], line: usize) -> Option<TranscriptChunk> {
    let index = chunks.partition_point(|chunk| chunk.start <= line);
    chunks
        .get(index.saturating_sub(1))
        .copied()
        .filter(|chunk| line < chunk.end)
}

fn render_follow_hint(app: &App, area: Rect, buf: &mut Buffer) {
    if app.viewport.shows_the_tail() || area.height == 0 {
        return;
    }
    let follow = if app.interaction.focus.is_log() {
        "G to follow"
    } else {
        "scroll down to follow"
    };
    let label = if app.viewport.has_unread() {
        format!(" new output below \u{b7} {follow} ")
    } else {
        format!(" {follow} ")
    };
    let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
    if width > area.width {
        return;
    }
    let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let x = row.x + (row.width - width) / 2;
    buf.set_string(
        x,
        row.y,
        &label,
        ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::REVERSED),
    );
    redraw_stitched_row(row, buf);
}

fn redraw_stitched_row(row: Rect, buf: &mut Buffer) {
    let stitched = row
        .columns()
        .any(|cell| buf[cell.as_position()].symbol().chars().count() > 1);
    if !stitched {
        return;
    }
    for cell in row.columns() {
        buf[cell.as_position()].set_diff_option(CellDiffOption::AlwaysUpdate);
    }
}

#[derive(Debug, Default)]
pub struct Screen {
    pub(crate) title: Option<String>,
    pub(crate) render_panic: Option<String>,
}

impl Screen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn cursor_cell(&self, app: &App, area: Rect) -> Option<(u16, u16)> {
        if !app.interaction.focus.is_log() || app.interaction.mode == crate::app::AppMode::Insert {
            return None;
        }
        let window = app_layout(app, area).messages;
        let column = u16::try_from(app.viewport.cursor().column).unwrap_or(u16::MAX);
        Some((
            window.x + column.min(window.width.saturating_sub(1)),
            window.y
                + app
                    .viewport
                    .cursor_row()
                    .min(window.height.saturating_sub(1)),
        ))
    }

    pub fn render(&mut self, app: &App, area: Rect, buf: &mut Buffer) {
        let regions = app_layout(app, area);
        app.input.area.set(regions.input);

        let transcript = transcript_guards(app, area.width);
        let lines = transcript.flatten();
        let window = app.viewport.place(&lines, regions.messages);
        for (row, line) in regions.messages.rows().zip(&lines[window.clone()]) {
            line.render(row, buf);
            redraw_stitched_row(row, buf);
        }
        self.paint_selection(app, regions.messages, window, buf);
        self.render_scrollbar(app, regions.messages, buf);
        render_follow_hint(app, regions.messages, buf);

        TopBar::new(app).render(regions.top, buf);
        crate::widgets::hint_line::render(app, regions.hint, buf);
        InputBox::new(app).render(regions.input, buf);
        StatusBar::new(app).render(regions.status, buf);
    }

    fn paint_selection(&self, app: &App, area: Rect, window: Range<usize>, buf: &mut Buffer) {
        for (row, line) in area.rows().zip(window) {
            let Some(columns) = app.viewport.selected_columns(line) else {
                continue;
            };
            for column in columns {
                let Ok(x) = u16::try_from(column) else {
                    continue;
                };
                if x >= row.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((row.x + x, row.y)) {
                    cell.set_style(app.theme.selection);
                }
            }
        }
    }

    fn render_scrollbar(&self, app: &App, area: Rect, buf: &mut Buffer) {
        let Some(thumb) = app.viewport.thumb(Instant::now()) else {
            return;
        };
        let x = area.right().saturating_sub(1);
        for row in thumb {
            let Ok(y) = u16::try_from(row) else { continue };
            if let Some(cell) = buf.cell_mut((x, area.y + y)) {
                cell.set_symbol("\u{2588}").set_style(app.theme.dim);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(message: usize, start: usize, end: usize) -> TranscriptChunk {
        TranscriptChunk {
            id: crate::widgets::message_list::TranscriptChunkId::Message(message),
            start,
            end,
        }
    }

    fn log_area(height: u16) -> Rect {
        Rect::new(0, 0, 80, height)
    }

    fn lines(widths: &[usize]) -> Vec<Line<'static>> {
        widths.iter().map(|w| Line::raw("x".repeat(*w))).collect()
    }

    fn place(viewport: &Viewport, lines: &[Line<'static>], area: Rect) -> Range<usize> {
        let references = lines.iter().collect::<Vec<_>>();
        viewport.place(
            &Transcript {
                lines: references,
                chunks: Vec::new(),
            },
            area,
        )
    }

    #[test]
    fn a_new_message_below_a_paused_view_is_unread_until_it_follows_again() {
        let viewport = Viewport::default();
        viewport.window(40, 10);
        viewport.remap_chunks(&[chunk(0, 0, 20), chunk(1, 20, 40)]);
        viewport.pause();
        viewport.scroll_by(-5);
        assert!(!viewport.is_following());

        viewport.remap_chunks(&[chunk(0, 0, 30), chunk(1, 30, 60)]);
        assert!(!viewport.has_unread(), "a rewrap is not new output");

        viewport.remap_chunks(&[chunk(0, 0, 30), chunk(1, 30, 60), chunk(2, 60, 70)]);
        viewport.window(70, 10);
        assert!(viewport.has_unread());
        assert!(!viewport.shows_the_tail());

        viewport.follow();
        viewport.window(70, 10);
        assert!(!viewport.has_unread());
        assert!(viewport.shows_the_tail());
    }

    #[test]
    fn resize_remaps_the_cursor_and_selection_anchor() {
        let viewport = Viewport::default();
        let previous = vec![chunk(0, 0, 10), chunk(1, 10, 20)];
        let current = vec![chunk(0, 0, 100), chunk(1, 100, 120)];
        viewport.window(20, 5);
        viewport.pause();
        viewport.scroll_by(-2);
        viewport.cursor_to(13);
        viewport.start_selection(SelectionKind::Char);
        viewport.chunks.replace(previous);

        viewport.remap_chunks(&current);

        assert_eq!(viewport.cursor().line, 103);
        assert_eq!(viewport.selection().unwrap().start().0, 103);
        assert_eq!(viewport.scroll_top.get(), 103);
    }

    #[test]
    fn wrapped_lines_remap_to_the_same_message() {
        let previous = [chunk(0, 0, 10), chunk(1, 10, 20)];
        let current = [chunk(0, 0, 100), chunk(1, 100, 120)];

        assert_eq!(remap_line(13, &previous, &current), 103);
    }

    #[test]
    fn wrapped_line_offsets_clamp_inside_the_message() {
        let previous = [chunk(0, 0, 20)];
        let current = [chunk(0, 0, 5)];

        assert_eq!(remap_line(13, &previous, &current), 4);
    }

    #[test]
    fn a_zero_height_area_leaves_the_cursor_on_the_last_line() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));

        let window = place(&viewport, &lines(&[10; 200]), log_area(0));

        assert!(window.is_empty());
        assert_eq!(viewport.cursor().line, 199);
    }

    #[test]
    fn the_cursor_can_be_put_on_a_column() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));
        viewport.cursor_to(190);
        viewport.move_cursor(7, 0);

        viewport.cursor_to_column(0);

        assert_eq!(viewport.cursor().column, 0);
    }

    #[test]
    fn a_blockwise_selection_covers_the_same_columns_on_every_row() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));
        viewport.cursor_to(190);
        viewport.move_cursor(3, 0);
        viewport.start_selection(SelectionKind::Block);

        viewport.move_cursor(3, 2);

        assert_eq!(viewport.selected_columns(190), Some(3..7));
        assert_eq!(viewport.selected_columns(191), Some(3..7));
        assert_eq!(viewport.selected_columns(192), Some(3..7));
        assert_eq!(viewport.selected_columns(193), None);
    }

    #[test]
    fn a_linewise_selection_covers_the_whole_row() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));
        viewport.cursor_to(190);
        viewport.move_cursor(3, 0);

        viewport.start_selection(SelectionKind::Line);

        assert_eq!(viewport.selected_columns(190), Some(0..10));
        assert_eq!(viewport.selected_columns(189), None);
    }

    #[test]
    fn a_fresh_selection_covers_the_cursor_cell() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));
        viewport.cursor_to(190);
        viewport.move_cursor(3, 0);

        viewport.start_selection(SelectionKind::Char);

        assert_eq!(viewport.selected_columns(190), Some(3..4));
        assert_eq!(viewport.selected_columns(189), None);
    }

    #[test]
    fn a_selection_over_several_lines_runs_each_to_its_end() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));
        viewport.cursor_to(190);
        viewport.move_cursor(3, 0);
        viewport.start_selection(SelectionKind::Char);

        viewport.move_cursor(0, 2);
        viewport.move_cursor(-2, 0);

        assert_eq!(viewport.selected_columns(190), Some(3..10));
        assert_eq!(viewport.selected_columns(191), Some(0..10));
        assert_eq!(viewport.selected_columns(192), Some(0..2));
        assert_eq!(viewport.selected_columns(193), None);
    }

    #[test]
    fn placing_records_the_log_area() {
        let viewport = Viewport::default();

        place(&viewport, &lines(&[10; 200]), Rect::new(0, 3, 80, 20));

        assert_eq!(viewport.area(), Rect::new(0, 3, 80, 20));
    }

    #[test]
    fn a_following_window_ends_at_the_newest_line() {
        assert_eq!(Viewport::default().window(200, 20), 180..200);
    }

    #[test]
    fn a_transcript_shorter_than_the_window_starts_at_the_top() {
        assert_eq!(Viewport::default().window(6, 20), 0..6);
    }

    #[test]
    fn scrolling_up_pauses_the_follow() {
        let viewport = Viewport::default();
        viewport.window(200, 20);

        viewport.scroll_by(-1);

        assert_eq!(viewport.window(200, 20), 179..199);
        assert!(!viewport.is_following());
    }

    #[test]
    fn scrolling_back_to_the_bottom_follows_again() {
        let viewport = Viewport::default();
        viewport.window(200, 20);
        viewport.scroll_by(-5);

        viewport.scroll_by(5);

        assert!(viewport.is_following());
    }

    #[test]
    fn leaving_the_log_scrolled_up_does_not_follow() {
        let viewport = Viewport::default();
        viewport.window(200, 20);
        viewport.pause();
        viewport.scroll_by(-5);

        viewport.resume();

        assert!(!viewport.is_following());
    }

    #[test]
    fn leaving_the_log_at_the_tail_follows_again() {
        let viewport = Viewport::default();
        viewport.window(200, 20);
        viewport.pause();

        viewport.resume();

        assert!(viewport.is_following());
    }

    #[test]
    fn the_scrollbar_fades_once_you_stop_moving() {
        let viewport = Viewport::default();
        viewport.window(200, 20);
        viewport.scroll_by(-10);

        let now = Instant::now();
        assert!(viewport.thumb(now).is_some());
        assert!(viewport.thumb(now + SCROLLBAR_FADE * 2).is_none());
    }

    #[test]
    fn a_transcript_that_fits_has_no_scrollbar() {
        let viewport = Viewport::default();
        viewport.window(6, 20);
        viewport.scroll_by(-1);

        assert!(viewport.thumb(Instant::now()).is_none());
    }

    #[test]
    fn a_paused_window_stays_put_as_the_transcript_grows() {
        let viewport = Viewport::default();
        viewport.window(200, 20);
        viewport.scroll_by(-10);

        assert_eq!(viewport.window(205, 20), 170..190);
    }

    #[test]
    fn pausing_holds_the_view_against_new_output() {
        let viewport = Viewport::default();
        viewport.window(200, 20);

        viewport.pause();

        assert_eq!(viewport.window(205, 20), 180..200);
    }

    #[test]
    fn the_cursor_starts_on_the_last_row_of_the_window() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));

        viewport.pause();

        assert_eq!(viewport.cursor().line, 199);
        assert_eq!(viewport.cursor_row(), 19);
    }

    #[test]
    fn the_cursor_stops_at_the_start_of_a_line() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));
        viewport.pause();

        viewport.move_cursor(-1, 0);

        assert_eq!(viewport.cursor().column, 0);
    }

    #[test]
    fn a_short_line_clamps_the_cursor_and_the_wanted_column_comes_back() {
        let viewport = Viewport::default();
        let transcript = lines(&[60, 12, 60]);
        place(&viewport, &transcript, log_area(20));
        viewport.pause();
        viewport.cursor_to(0);
        viewport.move_cursor(40, 0);

        viewport.move_cursor(0, 1);
        let clamped = viewport.cursor().column;
        viewport.move_cursor(0, 1);

        assert_eq!(clamped, 11);
        assert_eq!(viewport.cursor().column, 40);
    }

    #[test]
    fn moving_off_the_top_scrolls_one_line() {
        let viewport = Viewport::default();
        place(&viewport, &lines(&[10; 200]), log_area(20));
        viewport.pause();
        viewport.cursor_to(180);

        viewport.move_cursor(0, -1);

        assert_eq!(viewport.window(200, 20), 179..199);
        assert_eq!(viewport.cursor_row(), 0);
    }

    #[test]
    fn a_scroll_that_would_strand_the_cursor_parks_it() {
        let viewport = Viewport::default();
        let transcript = lines(&[10; 200]);
        place(&viewport, &transcript, log_area(20));
        viewport.pause();

        viewport.scroll_by(1);
        place(&viewport, &transcript, log_area(20));

        assert_eq!(viewport.cursor().line, 199);
        assert_eq!(viewport.cursor_row(), 19);
    }

    #[test]
    fn a_scroll_under_the_cursor_keeps_its_line() {
        let viewport = Viewport::default();
        let transcript = lines(&[10; 200]);
        place(&viewport, &transcript, log_area(20));
        viewport.pause();
        viewport.cursor_to(190);

        viewport.scroll_by(-1);

        assert_eq!(viewport.cursor().line, 190);
        assert_eq!(viewport.cursor_row(), 11);
    }
}
