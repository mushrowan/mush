mod abbrev;
mod fit;
mod segments;
#[cfg(test)]
mod tests;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::App;
use segments::{Segment, Side};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static BUILDS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_build_count() {
    BUILDS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn build_count() -> usize {
    BUILDS.with(Cell::get)
}

struct Rows {
    lines: Vec<Line<'static>>,
    timer: Option<String>,
}

fn rows(app: &App, width: u16) -> Rows {
    #[cfg(test)]
    BUILDS.with(|count| count.set(count.get() + 1));

    let built = segments::build(app);
    let width_cells = usize::from(width);
    let lines = if app.interaction.status_bar.rows(width) == 1 {
        let (mut left, mut right): (Vec<Segment>, Vec<Segment>) = built
            .identity
            .into_iter()
            .partition(|segment| segment.side == Side::Left);
        left.extend(built.activity);
        fit::fit(&mut left, &mut right, width_cells);
        vec![fit::compose(&left, &right, width_cells)]
    } else {
        let mut top = built.activity;
        fit::fit(&mut top, &mut [], width_cells);
        let (mut left, mut right): (Vec<Segment>, Vec<Segment>) = built
            .identity
            .into_iter()
            .partition(|segment| segment.side == Side::Left);
        fit::fit(&mut left, &mut right, width_cells);
        vec![
            fit::compose(&top, &[], width_cells),
            fit::compose(&left, &right, width_cells),
        ]
    };
    Rows {
        lines,
        timer: built.timer,
    }
}

fn ensure_status_bar_cache(app: &App, width: u16) {
    {
        let slot = app.render_state.status_bar_cache.borrow();
        if slot.as_ref().is_some_and(|cached| cached.width == width) {
            return;
        }
    }
    let Rows { lines, timer } = rows(app, width);
    *app.render_state.status_bar_cache.borrow_mut() = Some(crate::app::CachedStatusBar {
        width,
        rows: lines,
        cache_timer_text: timer,
    });
}

pub fn status_bar_height(app: &App, width: u16) -> u16 {
    app.interaction.status_bar.rows(width)
}

pub struct StatusBar<'a> {
    app: &'a App,
}

impl<'a> StatusBar<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        ensure_status_bar_cache(self.app, area.width);
        let cache = self.app.render_state.status_bar_cache.borrow();
        let Some(data) = cache.as_ref() else {
            return;
        };
        for (row, line) in area.rows().zip(&data.rows) {
            line.render(row, buf);
        }
        if let Some(timer) = data.cache_timer_text.as_deref() {
            let short = timer.split('/').next().unwrap_or(timer);
            if !mark_cache_timer_cells(timer, area, buf) {
                mark_cache_timer_cells(short, area, buf);
            }
        }
    }
}

fn mark_cache_timer_cells(timer: &str, area: Rect, buf: &mut Buffer) -> bool {
    if timer.is_empty() {
        return false;
    }
    let symbols: Vec<char> = timer.chars().collect();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if x as usize + symbols.len() > area.right() as usize {
                break;
            }
            let matches = symbols.iter().enumerate().all(|(offset, ch)| {
                let mut utf8 = [0; 4];
                buf[(x + offset as u16, y)].symbol() == ch.encode_utf8(&mut utf8)
            });
            if matches {
                for offset in 0..symbols.len() {
                    if let Some(cell) = buf.cell_mut((x + offset as u16, y)) {
                        cell.set_diff_option(CellDiffOption::AlwaysUpdate);
                    }
                }
                return true;
            }
        }
    }
    false
}
