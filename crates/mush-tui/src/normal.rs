use crate::motion::{self, Motion};
use crate::screen::SelectionKind;

pub(crate) mod edit;
pub(crate) mod keys;

#[derive(Debug, Default)]
pub(crate) struct NormalState {
    pub(crate) keys: keys::Keys,
    pub(crate) register: edit::Register,
    pub(crate) last_change: Option<edit::Change>,
    pub(crate) last_selection: LastSelection,
    pub(crate) marks: Marks,
}

#[derive(Debug, Default)]
pub(crate) struct Marks {
    pub(crate) composer: MarkTable,
    pub(crate) log: MarkTable,
}

#[derive(Debug, Default)]
pub(crate) struct MarkTable([Option<(usize, usize)>; 26]);

impl MarkTable {
    pub(crate) fn set(&mut self, name: char, at: (usize, usize)) {
        if let Some(slot) = Self::slot(name).and_then(|slot| self.0.get_mut(slot)) {
            *slot = Some(at);
        }
    }

    pub(crate) fn resolve(&self, command: keys::Command) -> Option<keys::Command> {
        let resolve = |motion| match motion {
            Motion::Mark { name, exact } => Some(Motion::Jump {
                to: (*self.0.get(Self::slot(name)?)?)?,
                linewise: !exact,
            }),
            motion => Some(motion),
        };
        Some(match command {
            keys::Command::Move(motion) => keys::Command::Move(resolve(motion)?),
            keys::Command::Operate(operator, keys::Target::Motion(motion)) => {
                keys::Command::Operate(operator, keys::Target::Motion(resolve(motion)?))
            }
            command => command,
        })
    }

    fn slot(name: char) -> Option<usize> {
        name.is_ascii_lowercase()
            .then(|| usize::from(name as u8 - b'a'))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: (usize, usize),
    pub(crate) head: (usize, usize),
    pub(crate) kind: SelectionKind,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LastSelection {
    pub(crate) composer: Option<Selection>,
    pub(crate) log: Option<Selection>,
}

pub(crate) trait NormalTarget {
    fn rows(&self) -> &[String];
    fn cursor(&self) -> (usize, usize);
    fn place_cursor(&mut self, at: (usize, usize));
    fn move_line(&mut self, delta: isize);
    fn start_selection(&mut self, kind: crate::screen::SelectionKind);
    fn clear_selection(&mut self);
    fn select(&mut self, selection: Selection);
    fn selection(&self) -> Option<Selection>;
    fn swap_selection_ends(&mut self);

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

pub(crate) fn select_object(
    target: &mut impl NormalTarget,
    object: motion::object::Object,
    count: Option<usize>,
) {
    if let Some(selection) =
        motion::object::selection(target.rows(), target.cursor(), object, count)
    {
        target.select(Selection {
            anchor: selection.anchor,
            head: selection.head,
            kind: if selection.linewise {
                SelectionKind::Line
            } else {
                SelectionKind::Char
            },
        });
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

    fn select(&mut self, selection: Selection) {
        self.viewport
            .select(selection.anchor, selection.head, selection.kind);
    }

    fn selection(&self) -> Option<Selection> {
        let (anchor, head) = self.viewport.selection_ends()?;
        Some(Selection {
            anchor,
            head,
            kind: self.viewport.selection_kind(),
        })
    }

    fn swap_selection_ends(&mut self) {
        self.viewport.swap_selection_ends();
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

pub(crate) fn composer_position(input: &crate::input_buffer::InputBuffer) -> (usize, usize) {
    byte_position(&input.text, input.cursor)
}

fn byte_position(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count();
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |row| row.chars().count());
    (line, column)
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

    fn position_of(&self, offset: usize) -> (usize, usize) {
        byte_position(&self.input.text, offset)
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
        composer_position(self.input)
    }

    fn place_cursor(&mut self, (line, column): (usize, usize)) {
        let line = line.min(self.rows.len().saturating_sub(1));
        let row = self.rows.get(line).map_or("", String::as_str);
        self.input.cursor = self.offset_of((line, column.min(motion::last_cell(row))));
    }

    fn move_line(&mut self, delta: isize) {
        let (line, column) = self.cursor();
        let goal = self.input.goal_column().unwrap_or(column);
        let line = line
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
        self.place_cursor((line, goal));
        self.input.hold_goal_column(goal);
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

    fn select(&mut self, selection: Selection) {
        let last = self.rows.len().saturating_sub(1);
        let clamp = |(line, column): (usize, usize)| (line.min(last), column);
        self.input.anchor = Some(self.offset_of(clamp(selection.anchor)));
        self.place_cursor(clamp(selection.head));
        self.input.selection_kind = match selection.kind {
            SelectionKind::Block => SelectionKind::Char,
            kind => kind,
        };
    }

    fn selection(&self) -> Option<Selection> {
        let anchor = self.input.anchor?;
        Some(Selection {
            anchor: self.position_of(anchor),
            head: self.cursor(),
            kind: self.input.selection_kind,
        })
    }

    fn swap_selection_ends(&mut self) {
        if let Some(anchor) = self.input.anchor.replace(self.input.cursor) {
            self.input.cursor = anchor;
        }
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
