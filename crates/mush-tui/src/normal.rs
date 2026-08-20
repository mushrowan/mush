use crate::keybinds::Action;
use crate::motion::{self, Word};

pub(crate) trait NormalTarget {
    fn rows(&self) -> &[String];
    fn cursor(&self) -> (usize, usize);
    fn place_cursor(&mut self, at: (usize, usize));
    fn move_line(&mut self, delta: isize);

    fn to_top(&mut self) {
        self.place_cursor((0, 0));
    }

    fn to_bottom(&mut self) {
        let last = self.rows().len().saturating_sub(1);
        self.place_cursor((last, 0));
    }
}

pub(crate) fn apply_motion(target: &mut impl NormalTarget, action: Action) -> bool {
    let at = target.cursor();
    let (line, column) = at;
    let row = target.rows().get(line).cloned().unwrap_or_default();
    match action {
        Action::CursorLineStart => target.place_cursor((line, 0)),
        Action::CursorFirstNonBlank => target.place_cursor((line, motion::first_non_blank(&row))),
        Action::CursorLineEnd => target.place_cursor((line, motion::last_cell(&row))),
        Action::CursorLeft => target.place_cursor((line, column.saturating_sub(1))),
        Action::CursorRight => target.place_cursor((line, column + 1)),
        Action::PreviousItem => target.move_line(-1),
        Action::NextItem => target.move_line(1),
        Action::NavigateTop => target.to_top(),
        Action::NavigateBottom => target.to_bottom(),
        Action::CursorWordForward => word(target, motion::word_forward, at, Word::Small),
        Action::CursorChunkForward => word(target, motion::word_forward, at, Word::Big),
        Action::CursorWordBack => word(target, motion::word_back, at, Word::Small),
        Action::CursorChunkBack => word(target, motion::word_back, at, Word::Big),
        Action::CursorWordEnd => word(target, motion::word_end, at, Word::Small),
        Action::CursorChunkEnd => word(target, motion::word_end, at, Word::Big),
        _ => return false,
    }
    true
}

fn word(target: &mut impl NormalTarget, motion: motion::Motion, at: (usize, usize), word: Word) {
    let to = motion(target.rows(), at, word);
    target.place_cursor(to);
}

pub(crate) struct LogTarget<'a> {
    rows: Vec<String>,
    viewport: &'a crate::screen::Viewport,
}

impl<'a> LogTarget<'a> {
    pub(crate) fn new(app: &'a crate::app::App) -> Self {
        Self {
            rows: crate::selection::transcript_rows(app),
            viewport: &app.viewport,
        }
    }
}

impl NormalTarget for LogTarget<'_> {
    fn rows(&self) -> &[String] {
        &self.rows
    }

    fn cursor(&self) -> (usize, usize) {
        let cursor = self.viewport.cursor();
        (cursor.line, cursor.column)
    }

    fn place_cursor(&mut self, (line, column): (usize, usize)) {
        self.viewport.cursor_to(line);
        self.viewport.cursor_to_column(column);
    }

    fn move_line(&mut self, delta: isize) {
        self.viewport.move_cursor(0, delta);
    }

    fn to_top(&mut self) {
        self.viewport.to_top();
    }

    fn to_bottom(&mut self) {
        self.viewport.to_bottom();
    }
}
