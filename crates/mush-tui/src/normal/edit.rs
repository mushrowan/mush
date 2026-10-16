use std::ops::{Range, RangeInclusive};

use crate::app::{App, AppEvent, AppMode};
use crate::input_buffer::{InputBuffer, PendingImage};
use crate::motion::{self, Region};

use super::keys::{Command, Entry, Operator, Target};
use super::{ComposerTarget, NormalTarget, composer_position};

const MAX_REPEAT: usize = 1_000;

#[derive(Debug, Clone, Default)]
pub(crate) struct Register {
    text: String,
    linewise: bool,
    images: Vec<PendingImage>,
}

pub(crate) fn run(app: &mut App, command: Command, count: Option<usize>) -> Option<AppEvent> {
    let event = match command {
        Command::Move(motion) => {
            let mut target = ComposerTarget::new(&mut app.input);
            super::apply_motion(&mut target, motion, count);
            None
        }
        Command::Scroll(_) => None,
        Command::Operate(operator, target) => operate(app, operator, target, count),
        Command::Put { before } if app.interaction.mode == AppMode::Visual => {
            put_over_selection(app, before, count);
            None
        }
        Command::Put { before } => {
            put(app, before, count);
            None
        }
        Command::Undo => {
            step_history(
                app,
                count,
                InputBuffer::undo,
                "already at the oldest change",
            );
            None
        }
        Command::Redo => {
            step_history(
                app,
                count,
                InputBuffer::redo,
                "already at the newest change",
            );
            None
        }
        Command::Insert(entry) => {
            insert_at(app, entry);
            None
        }
        Command::Select(object) => {
            super::select_object(&mut ComposerTarget::new(&mut app.input), object, count);
            None
        }
        Command::SwapEnds => {
            ComposerTarget::new(&mut app.input).swap_selection_ends();
            None
        }
    };
    app.input.ensure_cursor_visible();
    event
}

fn operate(
    app: &mut App,
    operator: Operator,
    target: Target,
    count: Option<usize>,
) -> Option<AppEvent> {
    let rows = rows(&app.input.text);
    let at = composer_position(&app.input);
    let region = match target {
        Target::Motion(motion) => {
            motion::span(&rows, at, motion, count, operator == Operator::Change)?
        }
        Target::Lines => {
            let last = rows.len().saturating_sub(1);
            let end = (at.0 + count.unwrap_or(1) - 1).min(last);
            Region::Lines(at.0..=end)
        }
        Target::Object(object) => motion::object::span(&rows, at, object, count)?,
        Target::Selection => {
            let region = selection_region(&app.input)?;
            app.interaction.mode = AppMode::Normal;
            app.input.cursor = app
                .input
                .anchor
                .map_or(app.input.cursor, |anchor| anchor.min(app.input.cursor));
            region
        }
    };
    match region {
        Region::Chars(range) => {
            let start = byte_offset(&app.input.text, range.start);
            let end = byte_offset(&app.input.text, range.end);
            on_chars(app, operator, start..end)
        }
        Region::Lines(lines) => on_lines(app, operator, &lines),
    }
}

fn on_chars(app: &mut App, operator: Operator, bytes: Range<usize>) -> Option<AppEvent> {
    if bytes.is_empty() {
        if operator == Operator::Change {
            app.input.cursor = bytes.start;
            enter_insert(app);
        }
        return None;
    }
    if operator == Operator::Yank {
        let event = yank(app, bytes.clone(), false, true);
        app.input.cursor = bytes.start;
        rest_on_a_character(&mut app.input);
        return event;
    }
    let images = app.input.take_images_in(bytes.clone());
    let text: String = app.input.text.drain(bytes.clone()).collect();
    app.interaction.normal.register = Register {
        text,
        linewise: false,
        images,
    };
    app.input.cursor = bytes.start;
    edited(app);
    if operator == Operator::Change {
        enter_insert(app);
    } else {
        rest_on_a_character(&mut app.input);
    }
    None
}

fn on_lines(app: &mut App, operator: Operator, lines: &RangeInclusive<usize>) -> Option<AppEvent> {
    let (start, end) = line_bounds(&app.input.text, lines);
    match operator {
        Operator::Yank => {
            let (_, column) = composer_position(&app.input);
            let event = yank(app, start..end, true, true);
            ComposerTarget::new(&mut app.input).place_cursor((*lines.start(), column));
            event
        }
        Operator::Delete => {
            let text = &app.input.text;
            let removal = if end < text.len() {
                start..end + 1
            } else if start > 0 {
                start - 1..end
            } else {
                start..end
            };
            let block = text[start..end].to_string();
            let images = app.input.take_images_in(removal.clone());
            app.input.text.drain(removal);
            app.interaction.normal.register = Register {
                text: block,
                linewise: true,
                images,
            };
            edited(app);
            let line = *lines.start();
            let mut target = ComposerTarget::new(&mut app.input);
            target.place_cursor((line, 0));
            super::apply_motion(&mut target, motion::Motion::FirstNonBlank, None);
            None
        }
        Operator::Change => {
            let indent = app.input.text[start..end]
                .chars()
                .take_while(|c| c.is_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
            let images = app.input.take_images_in(start..end);
            let block = app.input.text[start..end].to_string();
            app.input.text.drain(start + indent..end);
            app.interaction.normal.register = Register {
                text: block,
                linewise: true,
                images,
            };
            app.input.cursor = start + indent;
            edited(app);
            enter_insert(app);
            None
        }
    }
}

fn put(app: &mut App, before: bool, count: Option<usize>) {
    let register = app.interaction.normal.register.clone();
    if register.text.is_empty() && !register.linewise {
        return;
    }
    let times = count.unwrap_or(1).min(MAX_REPEAT);
    let images = (0..times)
        .flat_map(|_| register.images.iter().cloned())
        .collect();
    let (line, _) = composer_position(&app.input);
    if register.linewise {
        let block = vec![register.text.as_str(); times].join("\n");
        let (start, end) = line_bounds(&app.input.text, &(line..=line));
        let (at, block, line) = if before {
            (start, format!("{block}\n"), line)
        } else {
            (end, format!("\n{block}"), line + 1)
        };
        app.input.insert_with_images(at, &block, images);
        let mut target = ComposerTarget::new(&mut app.input);
        target.place_cursor((line, 0));
        super::apply_motion(&mut target, motion::Motion::FirstNonBlank, None);
    } else {
        let block = register.text.repeat(times);
        let cursor = app.input.cursor;
        let at = match app.input.text[cursor..].chars().next() {
            Some(c) if !before && c != '\n' => cursor + c.len_utf8(),
            _ => cursor,
        };
        app.input.insert_with_images(at, &block, images);
        app.input.cursor = if block.contains('\n') {
            at
        } else {
            at + block.len() - block.chars().next_back().map_or(0, char::len_utf8)
        };
    }
    edited(app);
}

fn insert_at(app: &mut App, entry: Entry) {
    let (line, _) = composer_position(&app.input);
    let (start, end) = line_bounds(&app.input.text, &(line..=line));
    let indent: String = app.input.text[start..end]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let input = &mut app.input;
    match entry {
        Entry::Before => {}
        Entry::After => {
            if let Some(c) = input.text[input.cursor..].chars().next()
                && c != '\n'
            {
                input.cursor += c.len_utf8();
            }
        }
        Entry::LineStart => input.cursor = start + indent.len(),
        Entry::LineEnd => input.cursor = end,
        Entry::Below => {
            input.text.insert_str(end, &format!("\n{indent}"));
            input.cursor = end + 1 + indent.len();
            edited(app);
        }
        Entry::Above => {
            input.text.insert_str(start, &format!("{indent}\n"));
            input.cursor = start + indent.len();
            edited(app);
        }
    }
    enter_insert(app);
}

fn step_history(
    app: &mut App,
    count: Option<usize>,
    step: fn(&mut InputBuffer) -> bool,
    exhausted: &str,
) {
    let mut moved = false;
    for _ in 0..count.unwrap_or(1) {
        if !step(&mut app.input) {
            break;
        }
        moved = true;
    }
    if !moved {
        app.status = Some(exhausted.into());
        return;
    }
    edited(app);
    rest_on_a_character(&mut app.input);
}

pub(crate) fn yank_selection(app: &mut App, strip: bool) -> Option<AppEvent> {
    let region = selection_region(&app.input)?;
    let start = app
        .input
        .anchor
        .map_or(app.input.cursor, |anchor| anchor.min(app.input.cursor));
    let (bytes, linewise) = region_bytes(&app.input.text, &region);
    let event = yank(app, bytes, linewise, strip);
    app.interaction.mode = AppMode::Normal;
    app.input.anchor = None;
    app.input.cursor = start;
    rest_on_a_character(&mut app.input);
    event
}

fn put_over_selection(app: &mut App, keep_register: bool, count: Option<usize>) {
    let Some(region) = selection_region(&app.input) else {
        return;
    };
    let register = app.interaction.normal.register.clone();
    if register.text.is_empty() && !register.linewise {
        return;
    }
    let (bytes, linewise) = region_bytes(&app.input.text, &region);
    let times = count.unwrap_or(1).min(MAX_REPEAT);
    let block = if register.linewise {
        vec![register.text.as_str(); times].join("\n")
    } else {
        register.text.repeat(times)
    };
    let block = if register.linewise && !linewise {
        format!("\n{block}\n")
    } else {
        block
    };
    let images = (0..times)
        .flat_map(|_| register.images.iter().cloned())
        .collect();
    let taken = app.input.take_images_in(bytes.clone());
    let text: String = app.input.text.drain(bytes.clone()).collect();
    app.input.insert_with_images(bytes.start, &block, images);
    if !keep_register {
        app.interaction.normal.register = Register {
            text,
            linewise,
            images: taken,
        };
    }
    app.interaction.mode = AppMode::Normal;
    edited(app);
    if register.linewise {
        let line = app.input.text[..bytes.start].matches('\n').count() + usize::from(!linewise);
        let mut target = ComposerTarget::new(&mut app.input);
        target.place_cursor((line, 0));
        super::apply_motion(&mut target, motion::Motion::FirstNonBlank, None);
    } else {
        app.input.cursor = if block.contains('\n') {
            bytes.start
        } else {
            bytes.start + block.len() - block.chars().next_back().map_or(0, char::len_utf8)
        };
    }
}

fn selection_region(input: &InputBuffer) -> Option<Region> {
    let anchor = input.anchor?;
    let (from, to) = (anchor.min(input.cursor), anchor.max(input.cursor));
    if input.selection_kind == crate::screen::SelectionKind::Line {
        let line = |byte: usize| input.text[..byte].matches('\n').count();
        return Some(Region::Lines(line(from)..=line(to)));
    }
    let end = input.text[to..]
        .chars()
        .next()
        .map_or(to, |c| to + c.len_utf8());
    let index = |byte: usize| input.text[..byte].chars().count();
    Some(Region::Chars(index(from)..index(end)))
}

fn region_bytes(text: &str, region: &Region) -> (Range<usize>, bool) {
    match region {
        Region::Chars(range) => (
            byte_offset(text, range.start)..byte_offset(text, range.end),
            false,
        ),
        Region::Lines(lines) => {
            let (start, end) = line_bounds(text, lines);
            (start..end, true)
        }
    }
}

fn yank(app: &mut App, bytes: Range<usize>, linewise: bool, strip: bool) -> Option<AppEvent> {
    let text = app.input.text[bytes.clone()].to_string();
    let copied = if strip {
        crate::selection::strip_padding(&text)
    } else {
        text.clone()
    };
    app.interaction.normal.register = Register {
        text,
        linewise,
        images: app.input.images_in(bytes),
    };
    Some(AppEvent::Copy { text: copied })
}

fn edited(app: &mut App) {
    app.completion.tab_state = None;
    app.completion.history = None;
    app.input.anchor = None;
}

fn enter_insert(app: &mut App) {
    app.input.anchor = None;
    app.interaction.mode = AppMode::Insert;
}

fn rest_on_a_character(input: &mut InputBuffer) {
    let at_line_end = input.text[input.cursor..]
        .chars()
        .next()
        .is_none_or(|c| c == '\n');
    if at_line_end {
        input.step_back_one_char();
    }
}

fn rows(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_string).collect()
}

fn byte_offset(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn line_bounds(text: &str, lines: &RangeInclusive<usize>) -> (usize, usize) {
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let start = starts.get(*lines.start()).copied().unwrap_or(text.len());
    let end = starts
        .get(lines.end() + 1)
        .map_or(text.len(), |next| next - 1);
    (start, end)
}
