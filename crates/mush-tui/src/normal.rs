use crate::motion::{self, Motion};

pub(crate) mod keys;

#[derive(Debug, Default)]
pub(crate) struct NormalState {
    pub(crate) keys: keys::Keys,
}

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

    fn to_line(&mut self, line: usize) {
        let last = self.rows().len().saturating_sub(1);
        self.place_cursor((line.min(last), 0));
    }
}

pub(crate) fn apply_motion(target: &mut impl NormalTarget, motion: Motion, count: Option<usize>) {
    let times = count.unwrap_or(1);
    match motion {
        Motion::Up => target.move_line(-(times as isize)),
        Motion::Down => target.move_line(times as isize),
        Motion::Top => match count {
            Some(line) => target.to_line(line.saturating_sub(1)),
            None => target.to_top(),
        },
        Motion::Bottom => match count {
            Some(line) => target.to_line(line.saturating_sub(1)),
            None => target.to_bottom(),
        },
        Motion::MatchPair if let Some(percent) = count => {
            if percent <= 100 {
                let lines = target.rows().len();
                target.to_line((percent * lines).div_ceil(100).saturating_sub(1));
            }
        }
        motion => {
            if let Some(to) = motion::destination(target.rows(), target.cursor(), motion, times) {
                target.place_cursor(to);
            }
        }
    }
}

pub(crate) struct LogTarget<'a> {
    rows: std::cell::OnceCell<Vec<String>>,
    app: &'a crate::app::App,
    viewport: &'a crate::screen::Viewport,
}

impl<'a> LogTarget<'a> {
    pub(crate) fn new(app: &'a crate::app::App) -> Self {
        Self {
            rows: std::cell::OnceCell::new(),
            app,
            viewport: &app.viewport,
        }
    }
}

impl NormalTarget for LogTarget<'_> {
    fn rows(&self) -> &[String] {
        self.rows
            .get_or_init(|| crate::selection::transcript_rows(self.app))
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

    fn to_line(&mut self, line: usize) {
        self.viewport.cursor_to(line);
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

impl ComposerTarget<'_> {
    pub(crate) fn delete_selection(&mut self) {
        let Some(selected) = self.input.selected_bytes() else {
            return;
        };
        self.input.text.drain(selected.clone());
        self.input.cursor = selected.start;
        self.input.anchor = None;
    }

    pub(crate) fn select_all(&mut self) {
        self.input.anchor = Some(0);
        self.input.selection_kind = crate::screen::SelectionKind::Line;
        self.input.cursor = self.input.text.len().saturating_sub(1);
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
