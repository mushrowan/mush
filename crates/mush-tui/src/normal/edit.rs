use std::ops::{Range, RangeInclusive};

use crate::app::{App, AppEvent, AppMode};
use crate::input_buffer::{IMAGE_PLACEHOLDER, InputBuffer, PendingImage};
use crate::motion::{self, Region};

use super::keys::{Command, Entry, Operator, Order, Target};
use super::{ComposerTarget, NormalTarget, composer_position};

const MAX_REPEAT: usize = 1_000;

#[derive(Debug, Clone, Default)]
pub(crate) struct Register {
    text: String,
    linewise: bool,
    images: Vec<PendingImage>,
}

impl Register {
    fn is_empty(&self) -> bool {
        self.text.is_empty() && !self.linewise
    }

    fn append(&mut self, more: Self) {
        if self.is_empty() {
            *self = more;
            return;
        }
        if self.linewise || more.linewise {
            self.text.push('\n');
            self.linewise = true;
        }
        self.text.push_str(&more.text);
        self.images.extend(more.images);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Registers {
    unnamed: Register,
    named: [Register; 26],
}

impl Registers {
    fn read(&self, name: Option<char>) -> Option<Register> {
        match name {
            None => Some(self.unnamed.clone()),
            Some(name) => Some(self.named[named_slot(name)?].clone()),
        }
    }

    fn write(&mut self, name: Option<char>, taken: Register) {
        match name.and_then(|name| Some((name, named_slot(name)?))) {
            Some((name, slot)) if name.is_ascii_uppercase() => {
                self.named[slot].append(taken);
                self.unnamed = self.named[slot].clone();
            }
            Some((_, slot)) => {
                self.named[slot] = taken.clone();
                self.unnamed = taken;
            }
            None if name == Some('_') => {}
            None => self.unnamed = taken,
        }
    }
}

fn named_slot(name: char) -> Option<usize> {
    name.is_ascii_alphabetic()
        .then(|| usize::from(name.to_ascii_lowercase() as u8 - b'a'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extent {
    Chars(usize),
    Lines(usize),
}

#[derive(Debug, Clone)]
pub(crate) struct Change {
    order: Order,
    selection: Option<Extent>,
    typed: Option<String>,
}

pub(crate) fn settle(app: &mut App) {
    take_typed(app);
    app.input.checkpoint();
}

pub(crate) fn leave_insert(app: &mut App) {
    let repeat = app.interaction.normal.insert_repeat.take();
    if let Some(typed) = take_typed(app)
        && let Some((entry, count)) = repeat
    {
        type_again(app, entry, &typed, count - 1);
    }
}

fn take_typed(app: &mut App) -> Option<String> {
    let typed = app.input.typed()?;
    if let Some(change) = app.interaction.normal.last_change.as_mut() {
        change.typed = Some(typed.clone());
    }
    Some(typed)
}

fn type_again(app: &mut App, entry: Entry, typed: &str, copies: usize) {
    let typed = typed.replace(IMAGE_PLACEHOLDER, "");
    let copies = copies.min(MAX_REPEAT);
    if typed.is_empty() || copies == 0 {
        return;
    }
    if matches!(entry, Entry::Below | Entry::Above) {
        let (line, _) = composer_position(&app.input);
        let (start, end) = line_bounds(&app.input.text, &(line..=line));
        let indent: String = app.input.text[start..end]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        let block: String = (0..copies).map(|_| format!("\n{indent}{typed}")).collect();
        app.input.text.insert_str(end, &block);
        app.input.cursor = end + block.len();
    } else {
        app.input.insert_str(&typed.repeat(copies));
    }
    edited(app);
}

pub(crate) fn run(app: &mut App, order: Order) -> Option<AppEvent> {
    let event = match order.command {
        Command::Repeat => repeat(app, order.count),
        command if repeatable(command) => {
            let before = app.input.text.clone();
            let selection = extent_of_selection(app);
            let event = execute(app, order);
            let typing = app.interaction.mode == AppMode::Insert;
            if typing || app.input.text != before {
                app.interaction.normal.last_change = Some(Change {
                    order,
                    selection,
                    typed: None,
                });
                if typing {
                    app.input.watch_typing();
                }
            }
            if typing
                && let Command::Insert(entry) = command
                && let Some(count @ 2..) = order.count
            {
                app.interaction.normal.insert_repeat = Some((entry, count));
            }
            event
        }
        _ => execute(app, order),
    };
    app.input.ensure_cursor_visible();
    event
}

pub(crate) fn put_text(app: &mut App, text: String, before: bool, count: Option<usize>) {
    let linewise = text.ends_with('\n');
    let register = Register {
        text: if linewise {
            text[..text.len() - 1].to_string()
        } else {
            text
        },
        linewise,
        images: Vec::new(),
    };
    if app.interaction.mode == AppMode::Visual {
        put_over_selection(app, register, before, count);
    } else {
        put(app, register, before, count);
    }
    app.input.ensure_cursor_visible();
}

fn repeatable(command: Command) -> bool {
    matches!(
        command,
        Command::Operate(Operator::Delete | Operator::Change, _)
            | Command::Put { .. }
            | Command::Insert(_)
            | Command::Replace(_)
            | Command::Join
            | Command::ToggleCase
    )
}

fn repeat(app: &mut App, count: Option<usize>) -> Option<AppEvent> {
    let mut change = app.interaction.normal.last_change.clone()?;
    if count.is_some() {
        change.order.count = count;
    }
    if let Some(extent) = change.selection {
        select_extent(app, extent);
    }
    let event = execute(app, change.order);
    if app.interaction.mode == AppMode::Insert {
        if let Some(typed) = &change.typed {
            app.input.insert_str(&typed.replace(IMAGE_PLACEHOLDER, ""));
            if let Command::Insert(entry) = change.order.command {
                let copies = change.order.count.unwrap_or(1) - 1;
                type_again(app, entry, typed, copies);
            }
        }
        app.input.step_back_one_char();
    }
    app.interaction.mode = AppMode::Normal;
    app.input.anchor = None;
    app.interaction.normal.last_change = Some(change);
    event
}

fn extent_of_selection(app: &App) -> Option<Extent> {
    if app.interaction.mode != AppMode::Visual {
        return None;
    }
    Some(match selection_region(&app.input)? {
        Region::Chars(range) => Extent::Chars(range.len()),
        Region::Lines(lines) => Extent::Lines(lines.end() - lines.start() + 1),
    })
}

fn select_extent(app: &mut App, extent: Extent) {
    let cursor = app.input.cursor;
    app.input.anchor = Some(cursor);
    app.interaction.mode = AppMode::Visual;
    match extent {
        Extent::Chars(chars) => {
            app.input.selection_kind = crate::screen::SelectionKind::Char;
            let text = &app.input.text;
            let start = motion::index_of_byte(text, cursor);
            app.input.cursor = motion::byte_of_index(text, start + chars.saturating_sub(1));
        }
        Extent::Lines(lines) => {
            app.input.selection_kind = crate::screen::SelectionKind::Line;
            ComposerTarget::new(&mut app.input).move_line(lines as isize - 1);
        }
    }
}

fn execute(app: &mut App, order: Order) -> Option<AppEvent> {
    let Some(command) = app.interaction.normal.marks.composer.resolve(order.command) else {
        app.status = Some("mark not set".into());
        return None;
    };
    let (count, register) = (order.count, order.register);
    let visible = ComposerTarget::new(&mut app.input).visible_lines();
    let command = super::resolve_screen(command, visible, count);
    let command = if super::wants_search(command) {
        let rows = rows(&app.input.text);
        let at = composer_position(&app.input);
        match super::resolve_search(command, &mut app.interaction.normal.keys, &rows, at, count) {
            Ok(command) => command,
            Err(message) => {
                app.status = Some(message.into());
                return None;
            }
        }
    } else {
        command
    };
    match command {
        Command::Move(motion) => {
            let mut target = ComposerTarget::new(&mut app.input);
            super::apply_motion(&mut target, motion, count);
            None
        }
        Command::Scroll(_) => None,
        Command::Operate(operator, target) => operate(app, operator, target, order),
        Command::Put { before } if register == Some('+') => {
            Some(AppEvent::PutClipboard { before, count })
        }
        Command::Put { before } => {
            let held = app.interaction.normal.registers.read(register)?;
            if app.interaction.mode == AppMode::Visual {
                put_over_selection(app, held, before, count);
            } else {
                put(app, held, before, count);
            }
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
        Command::Replace(with) => {
            replace(app, with, count);
            None
        }
        Command::Join => {
            join(app, count);
            None
        }
        Command::ToggleCase => {
            toggle_case(app, count);
            None
        }
        Command::Reselect => {
            if let Some(selection) = app.interaction.normal.last_selection.composer {
                ComposerTarget::new(&mut app.input).select(selection);
                app.interaction.mode = AppMode::Visual;
            }
            None
        }
        Command::SetMark(name) => {
            let at = composer_position(&app.input);
            app.interaction.normal.marks.composer.set(name, at);
            None
        }
        Command::Repeat => None,
    }
}

fn operate(app: &mut App, operator: Operator, target: Target, order: Order) -> Option<AppEvent> {
    let (count, register) = (order.count, order.register);
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
            on_chars(app, operator, start..end, register)
        }
        Region::Lines(lines) => on_lines(app, operator, &lines, register),
    }
}

fn on_chars(
    app: &mut App,
    operator: Operator,
    bytes: Range<usize>,
    register: Option<char>,
) -> Option<AppEvent> {
    if bytes.is_empty() {
        if operator == Operator::Change {
            app.input.cursor = bytes.start;
            enter_insert(app);
        }
        return None;
    }
    if operator == Operator::Yank {
        let event = yank(app, bytes.clone(), false, register, true);
        app.input.cursor = bytes.start;
        rest_on_a_character(&mut app.input);
        return event;
    }
    let images = app.input.take_images_in(bytes.clone());
    let text: String = app.input.text.drain(bytes.clone()).collect();
    let event = keep(
        app,
        register,
        Register {
            text,
            linewise: false,
            images,
        },
        None,
    );
    app.input.cursor = bytes.start;
    edited(app);
    if operator == Operator::Change {
        enter_insert(app);
    } else {
        rest_on_a_character(&mut app.input);
    }
    event
}

fn on_lines(
    app: &mut App,
    operator: Operator,
    lines: &RangeInclusive<usize>,
    register: Option<char>,
) -> Option<AppEvent> {
    let (start, end) = line_bounds(&app.input.text, lines);
    match operator {
        Operator::Yank => {
            let (_, column) = composer_position(&app.input);
            let event = yank(app, start..end, true, register, true);
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
            let event = keep(
                app,
                register,
                Register {
                    text: block,
                    linewise: true,
                    images,
                },
                None,
            );
            edited(app);
            let line = *lines.start();
            let mut target = ComposerTarget::new(&mut app.input);
            target.place_cursor((line, 0));
            super::apply_motion(&mut target, motion::Motion::FirstNonBlank, None);
            event
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
            let event = keep(
                app,
                register,
                Register {
                    text: block,
                    linewise: true,
                    images,
                },
                None,
            );
            app.input.cursor = start + indent;
            edited(app);
            enter_insert(app);
            event
        }
    }
}

fn put(app: &mut App, register: Register, before: bool, count: Option<usize>) {
    if register.is_empty() {
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
        let at = match after_grapheme(&app.input.text, cursor) {
            Some(end) if !before => end,
            _ => cursor,
        };
        app.input.insert_with_images(at, &block, images);
        app.input.cursor = if block.contains('\n') {
            at
        } else {
            at + crate::text::previous_grapheme(&block, block.len())
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
            if let Some(end) = after_grapheme(&input.text, input.cursor) {
                input.cursor = end;
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
    let event = yank(app, bytes, linewise, None, strip);
    app.interaction.mode = AppMode::Normal;
    app.input.anchor = None;
    app.input.cursor = start;
    rest_on_a_character(&mut app.input);
    event
}

fn put_over_selection(
    app: &mut App,
    register: Register,
    keep_register: bool,
    count: Option<usize>,
) {
    let Some(region) = selection_region(&app.input) else {
        return;
    };
    if register.is_empty() {
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
        app.interaction.normal.registers.write(
            None,
            Register {
                text,
                linewise,
                images: taken,
            },
        );
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
            bytes.start + crate::text::previous_grapheme(&block, block.len())
        };
    }
}

fn line_span(input: &InputBuffer, count: Option<usize>) -> Range<usize> {
    let cursor = input.cursor;
    let row = &input.text[cursor..];
    let row = &row[..row.find('\n').unwrap_or(row.len())];
    let end = crate::text::byte_of_grapheme(row, count.unwrap_or(1));
    cursor..cursor + end
}

fn after_grapheme(text: &str, at: usize) -> Option<usize> {
    let row = &text[at..];
    let row = &row[..row.find('\n').unwrap_or(row.len())];
    (!row.is_empty()).then(|| at + crate::text::byte_of_grapheme(row, 1))
}

fn selected_bytes(app: &mut App) -> Option<Range<usize>> {
    if app.interaction.mode != AppMode::Visual {
        return None;
    }
    let region = selection_region(&app.input)?;
    let (bytes, _) = region_bytes(&app.input.text, &region);
    app.interaction.mode = AppMode::Normal;
    Some(bytes)
}

fn rewrite(app: &mut App, bytes: Range<usize>, change: impl Fn(&str) -> String) -> usize {
    let rewritten: String = crate::text::graphemes(&app.input.text[bytes.clone()])
        .map(|(_, grapheme)| change(grapheme))
        .collect();
    let placeholder = IMAGE_PLACEHOLDER.to_string();
    if change(&placeholder) != placeholder {
        app.input.take_images_in(bytes.clone());
    }
    app.input.text.replace_range(bytes, &rewritten);
    edited(app);
    rewritten.len()
}

fn replace(app: &mut App, with: char, count: Option<usize>) {
    if let Some(bytes) = selected_bytes(app) {
        let start = bytes.start;
        rewrite(app, bytes, |grapheme| {
            if grapheme == "\n" {
                grapheme.to_string()
            } else {
                with.to_string()
            }
        });
        app.input.cursor = start;
        return;
    }
    let times = count.unwrap_or(1);
    let bytes = line_span(&app.input, count);
    if crate::text::grapheme_count(&app.input.text[bytes.clone()]) < times {
        return;
    }
    let start = bytes.start;
    rewrite(app, bytes, |_| with.to_string());
    app.input.cursor = start + (times - 1) * with.len_utf8();
}

fn toggle_case(app: &mut App, count: Option<usize>) {
    let flip = |grapheme: &str| {
        grapheme
            .chars()
            .flat_map(|c| -> Vec<char> {
                if c.is_lowercase() {
                    c.to_uppercase().collect()
                } else if c.is_uppercase() {
                    c.to_lowercase().collect()
                } else {
                    vec![c]
                }
            })
            .collect::<String>()
    };
    if let Some(bytes) = selected_bytes(app) {
        let start = bytes.start;
        rewrite(app, bytes, flip);
        app.input.cursor = start;
        return;
    }
    let bytes = line_span(&app.input, count);
    let start = bytes.start;
    app.input.cursor = start + rewrite(app, bytes, flip);
    rest_on_a_character(&mut app.input);
}

fn join(app: &mut App, count: Option<usize>) {
    let (first, lines) = match selected_bytes(app) {
        Some(bytes) => {
            let line = |byte: usize| app.input.text[..byte].matches('\n').count();
            let (first, last) = (line(bytes.start), line(bytes.end));
            (first, last - first + 1)
        }
        None => (composer_position(&app.input).0, count.unwrap_or(2)),
    };
    let total = app.input.text.matches('\n').count() + 1;
    if first + 1 >= total {
        return;
    }
    let joins = lines.max(2).min(total - first) - 1;
    for _ in 0..joins {
        let (start, end) = line_bounds(&app.input.text, &(first..=first));
        let text = &app.input.text;
        let next = &text[end + 1..];
        let next = &next[..next.find('\n').unwrap_or(next.len())];
        let indent = next.len() - next.trim_start_matches([' ', '\t']).len();
        let rest = &next[indent..];
        let current = &text[start..end];
        let space = !rest.is_empty()
            && !rest.starts_with(')')
            && !current.is_empty()
            && !current.ends_with([' ', '\t']);
        app.input
            .text
            .replace_range(end..end + 1 + indent, if space { " " } else { "" });
        app.input.cursor = end;
    }
    edited(app);
    rest_on_a_character(&mut app.input);
}

fn selection_region(input: &InputBuffer) -> Option<Region> {
    let anchor = input.anchor?;
    let (from, to) = (anchor.min(input.cursor), anchor.max(input.cursor));
    if input.selection_kind == crate::screen::SelectionKind::Line {
        let line = |byte: usize| input.text[..byte].matches('\n').count();
        return Some(Region::Lines(line(from)..=line(to)));
    }
    let index = |byte: usize| motion::index_of_byte(&input.text, byte);
    let end = if to < input.text.len() {
        index(to) + 1
    } else {
        index(to)
    };
    Some(Region::Chars(index(from)..end))
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

fn yank(
    app: &mut App,
    bytes: Range<usize>,
    linewise: bool,
    register: Option<char>,
    strip: bool,
) -> Option<AppEvent> {
    let taken = Register {
        text: app.input.text[bytes.clone()].to_string(),
        linewise,
        images: app.input.images_in(bytes),
    };
    keep(app, register, taken, Some(strip))
}

fn keep(
    app: &mut App,
    register: Option<char>,
    taken: Register,
    yanked_with_strip: Option<bool>,
) -> Option<AppEvent> {
    let copied = match (register, yanked_with_strip) {
        (Some('+'), _) | (None, Some(false)) => Some(taken.text.clone()),
        (None, Some(true)) => Some(crate::selection::strip_padding(&taken.text)),
        _ => None,
    };
    app.interaction.normal.registers.write(register, taken);
    copied.map(|text| AppEvent::Copy { text })
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
    motion::byte_of_index(text, index)
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
