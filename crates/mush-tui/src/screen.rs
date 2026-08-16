use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::App;
use crate::ui::{input_height, layout, status_bar_height};
use crate::widgets::input_box::InputBox;
use crate::widgets::message_list::transcript_guards;
use crate::widgets::status_bar::StatusBar;
use crate::widgets::tool_panels::{ToolPanels, tool_panels_height};

pub const SCROLLBAR_FADE: Duration = Duration::from_millis(1200);

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
    total: Cell<usize>,
    height: Cell<usize>,
    moved: Cell<Option<Instant>>,
    cursor: Cell<Cursor>,
    widths: RefCell<Vec<usize>>,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            scroll_top: Cell::new(0),
            following: Cell::new(true),
            total: Cell::new(0),
            height: Cell::new(0),
            moved: Cell::new(None),
            cursor: Cell::new(Cursor::default()),
            widths: RefCell::new(Vec::new()),
        }
    }
}

impl Viewport {
    pub fn place<L: std::borrow::Borrow<Line<'static>>>(
        &self,
        lines: &[L],
        height: usize,
    ) -> Range<usize> {
        let window = self.window(lines.len(), height);
        self.widths.replace(
            lines[window.clone()]
                .iter()
                .map(|line| {
                    line.borrow()
                        .spans
                        .iter()
                        .map(|s| s.content.chars().count())
                        .sum()
                })
                .collect(),
        );
        self.hold_cursor_on_screen();
        window
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
        top..total.min(top + height)
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

    pub fn cursor_to(&self, line: usize) {
        let mut cursor = self.cursor.get();
        cursor.line = line.min(self.total.get().saturating_sub(1));
        self.cursor.set(cursor);
        self.keep_in_view(cursor.line);
        self.clamp_cursor();
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

    fn window_end(&self) -> usize {
        self.total.get().min(self.scroll_top.get() + self.page())
    }

    fn max_scroll(&self) -> usize {
        self.total.get().saturating_sub(self.height.get())
    }
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
pub struct Screen {}

impl Screen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn cursor_cell(&self, app: &App, area: Rect) -> Option<(u16, u16)> {
        if app.interaction.mode != crate::app::AppMode::Navigate {
            return None;
        }
        let tools_h = tool_panels_height(&app.active_tools, area.width);
        let input_h = input_height(&app.input, area.width);
        let status_h = status_bar_height(app, area.width);
        let window = layout(area, input_h, tools_h, status_h).messages;
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
        let tools_h = tool_panels_height(&app.active_tools, area.width);
        let input_h = input_height(&app.input, area.width);
        let status_h = status_bar_height(app, area.width);
        let regions = layout(area, input_h, tools_h, status_h);
        app.input.area.set(regions.input);

        let transcript = transcript_guards(app, area.width);
        let lines = transcript.flatten();
        let window = app
            .viewport
            .place(&lines, usize::from(regions.messages.height));
        for (row, line) in regions.messages.rows().zip(&lines[window]) {
            line.render(row, buf);
            redraw_stitched_row(row, buf);
        }
        self.render_scrollbar(app, regions.messages, buf);

        if let Some(tools) = regions.tools {
            ToolPanels::new(&app.active_tools, &app.theme).render(tools, buf);
        }
        InputBox::new(app).render(regions.input, buf);
        StatusBar::new(app).render(regions.status, buf);
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

    fn lines(widths: &[usize]) -> Vec<Line<'static>> {
        widths.iter().map(|w| Line::raw("x".repeat(*w))).collect()
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
        viewport.place(&lines(&[10; 200]), 20);

        viewport.pause();

        assert_eq!(viewport.cursor().line, 199);
        assert_eq!(viewport.cursor_row(), 19);
    }

    #[test]
    fn the_cursor_stops_at_the_start_of_a_line() {
        let viewport = Viewport::default();
        viewport.place(&lines(&[10; 200]), 20);
        viewport.pause();

        viewport.move_cursor(-1, 0);

        assert_eq!(viewport.cursor().column, 0);
    }

    #[test]
    fn a_short_line_clamps_the_cursor_and_the_wanted_column_comes_back() {
        let viewport = Viewport::default();
        let transcript = lines(&[60, 12, 60]);
        viewport.place(&transcript, 20);
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
        viewport.place(&lines(&[10; 200]), 20);
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
        viewport.place(&transcript, 20);
        viewport.pause();

        viewport.scroll_by(1);
        viewport.place(&transcript, 20);

        assert_eq!(viewport.cursor().line, 199);
        assert_eq!(viewport.cursor_row(), 19);
    }

    #[test]
    fn a_scroll_under_the_cursor_keeps_its_line() {
        let viewport = Viewport::default();
        let transcript = lines(&[10; 200]);
        viewport.place(&transcript, 20);
        viewport.pause();
        viewport.cursor_to(190);

        viewport.scroll_by(-1);

        assert_eq!(viewport.cursor().line, 190);
        assert_eq!(viewport.cursor_row(), 11);
    }
}
