use crate::motion::{self, Motion};
use crate::screen::SelectionKind;

pub(crate) mod edit;
pub(crate) mod keys;

#[derive(Debug, Default)]
pub(crate) struct NormalState {
    pub(crate) keys: keys::Keys,
    pub(crate) registers: edit::Registers,
    pub(crate) last_change: Option<edit::Change>,
    pub(crate) last_selection: LastSelection,
    pub(crate) marks: Marks,
    pub(crate) insert_repeat: Option<(keys::Entry, usize)>,
}

#[derive(Debug, Default)]
pub(crate) struct Marks {
    pub(crate) composer: MarkTable,
}

#[derive(Debug, Default)]
pub(crate) struct MarkTable([Option<(usize, usize)>; 26]);

impl MarkTable {
    pub(crate) fn set(&mut self, name: char, at: (usize, usize)) {
        if let Some(slot) = Self::slot(name).and_then(|slot| self.0.get_mut(slot)) {
            *slot = Some(at);
        }
    }

    pub(crate) fn remap(&mut self, line: impl Fn(usize) -> usize) {
        for (at, _) in self.0.iter_mut().flatten() {
            *at = line(*at);
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
    fn visible_lines(&self) -> (usize, usize);

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

    fn line_count(&self) -> usize {
        self.rows().len()
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
                let lines = target.line_count();
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
    let grown = target
        .selection()
        .filter(|current| current.anchor != current.head)
        .map(|current| {
            motion::object::grow(target.rows(), (current.anchor, current.head), object, count)
        });
    let selection = match grown {
        Some(grown) => grown,
        None => motion::object::selection(target.rows(), target.cursor(), object, count),
    };
    if let Some(selection) = selection {
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

pub(crate) fn resolve_screen(
    command: keys::Command,
    (top, bottom): (usize, usize),
    count: Option<usize>,
) -> keys::Command {
    let resolve = |motion| {
        let Motion::Screen(screen) = motion else {
            return motion;
        };
        let skip = count.unwrap_or(1) - 1;
        Motion::Line(match screen {
            motion::Screen::Top => (top + skip).min(bottom),
            motion::Screen::Middle => top + (bottom - top) / 2,
            motion::Screen::Bottom => bottom.saturating_sub(skip).max(top),
        })
    };
    match command {
        keys::Command::Move(motion) => keys::Command::Move(resolve(motion)),
        keys::Command::Operate(operator, keys::Target::Motion(motion)) => {
            keys::Command::Operate(operator, keys::Target::Motion(resolve(motion)))
        }
        command => command,
    }
}

pub(crate) fn wants_search(command: keys::Command) -> bool {
    matches!(
        command,
        keys::Command::Move(Motion::Search { .. } | Motion::SearchWord { .. })
            | keys::Command::Operate(
                _,
                keys::Target::Motion(Motion::Search { .. } | Motion::SearchWord { .. })
            )
    )
}

pub(crate) fn resolve_search(
    command: keys::Command,
    keys: &mut keys::Keys,
    rows: &[String],
    at: (usize, usize),
    count: Option<usize>,
) -> Result<keys::Command, String> {
    let motion = match command {
        keys::Command::Move(motion) | keys::Command::Operate(_, keys::Target::Motion(motion)) => {
            motion
        }
        command => return Ok(command),
    };
    let (pattern, forward) = match motion {
        Motion::Search { reverse } => {
            let search = keys.last_search().ok_or("no earlier search")?;
            (search.pattern.clone(), search.forward != reverse)
        }
        Motion::SearchWord { forward } => {
            let pattern = motion::word_pattern(rows, at).ok_or("no word under the cursor")?;
            keys.remember_search(keys::Search {
                pattern: pattern.clone(),
                forward,
            });
            (pattern, forward)
        }
        _ => return Ok(command),
    };
    let to = motion::search(rows, at, &pattern, forward, count.unwrap_or(1))
        .ok_or_else(|| format!("pattern not found: {pattern}"))?;
    let jump = Motion::Jump {
        to,
        linewise: false,
    };
    Ok(match command {
        keys::Command::Operate(operator, _) => {
            keys::Command::Operate(operator, keys::Target::Motion(jump))
        }
        _ => keys::Command::Move(jump),
    })
}

const LOG_REACH: usize = 400;

pub(crate) struct LogTarget<'a> {
    rows: Vec<String>,
    base: usize,
    total: usize,
    viewport: &'a crate::screen::Viewport,
}

impl<'a> LogTarget<'a> {
    pub(crate) fn new(app: &'a crate::app::App) -> Self {
        Self::reaching(app, None)
    }

    pub(crate) fn reaching(app: &'a crate::app::App, line: Option<usize>) -> Self {
        let reach = (app.viewport.page() * 4).max(LOG_REACH);
        let rows = crate::widgets::message_list::transcript::rows(app, |viewport| {
            let anchor = viewport.selection_ends().map(|(anchor, _)| anchor.0);
            let cursor = viewport.cursor().line;
            let lines = [Some(cursor), anchor, line];
            let low = lines.iter().flatten().min().copied().unwrap_or(cursor);
            let high = lines.iter().flatten().max().copied().unwrap_or(cursor);
            low.saturating_sub(reach)..high.saturating_add(reach + 1)
        });
        Self {
            rows: rows.rows,
            base: rows.base,
            total: rows.total,
            viewport: &app.viewport,
        }
    }

    fn local(&self, (line, column): (usize, usize)) -> (usize, usize) {
        (line.saturating_sub(self.base), column)
    }

    fn global(&self, (line, column): (usize, usize)) -> (usize, usize) {
        (line + self.base, column)
    }

    pub(crate) fn localise(&self, command: keys::Command) -> keys::Command {
        let localise = |motion| match motion {
            Motion::Jump { to, linewise } => Motion::Jump {
                to: self.local(to),
                linewise,
            },
            Motion::Line(line) => Motion::Line(line.saturating_sub(self.base)),
            motion => motion,
        };
        match command {
            keys::Command::Move(motion) => keys::Command::Move(localise(motion)),
            keys::Command::Operate(operator, keys::Target::Motion(motion)) => {
                keys::Command::Operate(operator, keys::Target::Motion(localise(motion)))
            }
            command => command,
        }
    }
}

impl NormalTarget for LogTarget<'_> {
    fn rows(&self) -> &[String] {
        &self.rows
    }

    fn cursor(&self) -> (usize, usize) {
        let cursor = self.viewport.cursor();
        self.local((cursor.line, cursor.column))
    }

    fn place_cursor(&mut self, at: (usize, usize)) {
        let (line, column) = self.global(at);
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
        self.viewport.select(
            self.global(selection.anchor),
            self.global(selection.head),
            selection.kind,
        );
    }

    fn selection(&self) -> Option<Selection> {
        let (anchor, head) = self.viewport.selection_ends()?;
        Some(Selection {
            anchor: self.local(anchor),
            head: self.local(head),
            kind: self.viewport.selection_kind(),
        })
    }

    fn swap_selection_ends(&mut self) {
        self.viewport.swap_selection_ends();
    }

    fn visible_lines(&self) -> (usize, usize) {
        let (top, bottom) = self.viewport.visible_lines();
        (
            top.saturating_sub(self.base),
            bottom.saturating_sub(self.base),
        )
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

    fn line_count(&self) -> usize {
        self.total
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
        .map_or(0, crate::text::grapheme_count);
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
        offset + crate::text::byte_of_grapheme(row, column)
    }

    fn cells_at(&self, (line, column): (usize, usize)) -> usize {
        let row = self.rows.get(line).map_or("", String::as_str);
        crate::text::width_before_grapheme(row, column)
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
        let at = self.cursor();
        let goal = self
            .input
            .goal_column()
            .unwrap_or_else(|| self.cells_at(at));
        let line =
            at.0.saturating_add_signed(delta)
                .min(self.rows.len().saturating_sub(1));
        let row = self.rows.get(line).map_or("", String::as_str);
        self.place_cursor((line, crate::text::grapheme_at_cell(row, goal)));
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

    fn visible_lines(&self) -> (usize, usize) {
        let last = self.rows.len().saturating_sub(1);
        let width = usize::from(self.input.area.get().width.saturating_sub(2));
        let height = usize::from(self.input.visible_lines.get());
        if width == 0 || height == 0 {
            return (0, last);
        }
        let layout = self.input.layout(width);
        let Some(final_row) = layout.wrapped_lines.len().checked_sub(1) else {
            return (0, last);
        };
        let scroll = usize::from(self.input.scroll.get());
        let line_at = |row: usize| {
            layout.expanded.text[..layout.wrapped_lines[row.min(final_row)].start]
                .matches('\n')
                .count()
        };
        (line_at(scroll), line_at(scroll + height - 1))
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
