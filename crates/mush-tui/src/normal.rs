use crate::keybinds::Action;
use crate::motion::{self, Word};

pub(crate) trait NormalTarget {
    fn rows(&self) -> &[String];
    fn cursor(&self) -> (usize, usize);
    fn place_cursor(&mut self, at: (usize, usize));
    fn move_line(&mut self, delta: isize);
    fn start_selection(&mut self, kind: crate::screen::SelectionKind);
    fn clear_selection(&mut self);

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

    fn start_selection(&mut self, kind: crate::screen::SelectionKind) {
        self.viewport.start_selection(kind);
    }

    fn clear_selection(&mut self) {
        self.viewport.clear_selection();
    }

    fn to_top(&mut self) {
        self.viewport.to_top();
    }

    fn to_bottom(&mut self) {
        self.viewport.to_bottom();
    }
}

pub(crate) struct ComposerTarget<'a> {
    rows: Vec<String>,
    input: &'a mut crate::input_buffer::InputBuffer,
}

impl<'a> ComposerTarget<'a> {
    pub(crate) fn new(input: &'a mut crate::input_buffer::InputBuffer) -> Self {
        let rows = input.text.split('\n').map(str::to_string).collect();
        Self { rows, input }
    }

    fn offset_of(&self, (line, column): (usize, usize)) -> usize {
        let mut offset = 0;
        for row in self.rows.iter().take(line) {
            offset += row.len() + 1;
        }
        let row = self.rows.get(line).map_or("", String::as_str);
        offset
            + row
                .char_indices()
                .nth(column)
                .map_or_else(|| row.len(), |(byte, _)| byte)
    }
}

impl NormalTarget for ComposerTarget<'_> {
    fn rows(&self) -> &[String] {
        &self.rows
    }

    fn cursor(&self) -> (usize, usize) {
        let before = &self.input.text[..self.input.cursor];
        let line = before.matches('\n').count();
        let column = before
            .rsplit('\n')
            .next()
            .map_or(0, |row| row.chars().count());
        (line, column)
    }

    fn place_cursor(&mut self, at: (usize, usize)) {
        self.input.cursor = self.offset_of(at);
    }

    fn move_line(&mut self, delta: isize) {
        let (line, column) = self.cursor();
        let line = line
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
        self.place_cursor((line, column));
    }

    fn start_selection(&mut self, kind: crate::screen::SelectionKind) {
        self.input.anchor = Some(self.input.cursor);
        self.input.selection_kind = match kind {
            crate::screen::SelectionKind::Block => crate::screen::SelectionKind::Char,
            kind => kind,
        };
    }

    fn clear_selection(&mut self) {
        self.input.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_buffer::InputBuffer;

    #[test]
    fn the_composer_reads_as_logical_rows_with_a_char_cursor() {
        let mut input = InputBuffer::new();
        input.text = "hello world\nsecond".into();
        input.cursor = 6;

        let target = ComposerTarget::new(&mut input);

        assert_eq!(target.rows(), ["hello world".to_string(), "second".into()]);
        assert_eq!(target.cursor(), (0, 6));
    }

    #[test]
    fn placing_the_composer_cursor_maps_back_to_a_byte_offset() {
        let mut input = InputBuffer::new();
        input.text = "hello world\nsecond".into();
        let mut target = ComposerTarget::new(&mut input);

        target.place_cursor((1, 2));

        assert_eq!(input.cursor, 14);
    }
}
