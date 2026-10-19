use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};

use mush_runtime::{Answer, DecisionStatus, Draft};

use crate::app::App;
use crate::decisions::{Chip, Editor, EditorKind, Group, Item, ItemKey, chip, groups, outgoing};

const WIDE: u16 = 110;
const ID_WIDTH: usize = 8;
const DETAIL_INDENT: usize = 6;

fn width_of(text: &str) -> usize {
    if text.len() == 1 && text.as_bytes()[0].is_ascii_control() {
        return 0;
    }
    usize::from(text.cell_width())
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| width_of(&span.content)).sum()
}

fn truncate(text: &str, width: usize) -> String {
    let clean;
    let text = if text.chars().any(char::is_control) {
        clean = text.chars().filter(|c| !c.is_control()).collect::<String>();
        clean.as_str()
    } else {
        text
    };
    if width_of(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for c in text.chars() {
        let mut buf = [0; 4];
        if width_of(&out) + width_of(c.encode_utf8(&mut buf)) + 1 > width {
            break;
        }
        out.push(c);
    }
    let mut out = out.trim_end().to_owned();
    out.push('\u{2026}');
    out
}

fn pieces(word: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut piece = String::new();
    for c in word.chars().filter(|c| !c.is_control()) {
        let mut buf = [0; 4];
        let c = c.encode_utf8(&mut buf);
        if !piece.is_empty() && width_of(&piece) + width_of(c) > width {
            out.push(std::mem::take(&mut piece));
        }
        piece.push_str(c);
    }
    out.push(piece);
    out
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph
            .split_whitespace()
            .flat_map(|word| pieces(word, width))
        {
            if line.is_empty() {
                line = word;
            } else if width_of(&line) + 1 + width_of(&word) <= width {
                line.push(' ');
                line.push_str(&word);
            } else {
                lines.push(std::mem::take(&mut line));
                line = word;
            }
        }
        lines.push(line);
    }
    lines
}

fn truncate_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let span_width = width_of(&span.content);
        if used + span_width <= width {
            used += span_width;
            out.push(span);
            continue;
        }
        let text = truncate(&span.content, width.saturating_sub(used));
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        break;
    }
    out
}

fn pad(mut spans: Vec<Span<'static>>, width: usize, style: Style) -> Vec<Span<'static>> {
    let used = spans_width(&spans);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    spans
}

fn selected_key(app: &App, groups: &[Group<'_>]) -> Option<ItemKey> {
    let keys = crate::decisions::flat(groups);
    app.decisions_panel
        .selected
        .clone()
        .filter(|key| keys.contains(key))
        .or_else(|| keys.first().cloned())
}

fn chip_spans(app: &App, chip: &Chip) -> Vec<Span<'static>> {
    let theme = &app.theme;
    let warn = theme.context_warn.add_modifier(Modifier::BOLD);
    match chip {
        Chip::Open => vec![Span::styled("\u{25cb}", theme.dim)],
        Chip::OpenText => vec![Span::styled("\u{25cb} text", theme.dim)],
        Chip::Picked { letter, lean } => {
            let text = if *lean {
                format!("\u{25cf} {letter} lean")
            } else {
                format!("\u{25cf} {letter}")
            };
            vec![Span::styled(text, theme.context_ok)]
        }
        Chip::Own => vec![Span::styled("\u{270e} own", theme.status_model)],
        Chip::Clarify => vec![Span::styled("? clarify", warn)],
        Chip::Later => vec![Span::styled("\u{25cc} later", theme.dim)],
        Chip::Changed => vec![Span::styled("\u{21bb} changed", warn)],
        Chip::Asked => vec![Span::styled("? asked", theme.dim)],
        Chip::Updated => vec![Span::styled("\u{21bb} updated", theme.context_warn)],
        Chip::OnLean => vec![Span::styled("\u{29d7} on lean", theme.dim)],
        Chip::Allow(true) => vec![Span::styled("\u{25cf} allow", theme.context_ok)],
        Chip::Allow(false) => vec![Span::styled("\u{25cf} deny", theme.context_danger)],
    }
}

fn age(item: &Item<'_>) -> String {
    match item {
        Item::Decision(decision) => decision.asked_at.age_display(),
        Item::Confirm(child) => child.updated_at.age_display(),
    }
}

fn item_row(
    app: &App,
    item: &Item<'_>,
    index: usize,
    selected: bool,
    expanded: bool,
    width: usize,
) -> (Line<'static>, bool) {
    let theme = &app.theme;
    let base = if selected {
        Style::default().bg(theme.menu_selected_bg)
    } else {
        Style::default()
    };
    let marker = if selected && expanded {
        Span::styled("\u{25be} ", theme.status_model.patch(base))
    } else if selected {
        Span::styled("\u{25b8} ", theme.status_model.patch(base))
    } else if item.blocking() {
        Span::styled("! ", theme.context_danger.add_modifier(Modifier::BOLD))
    } else {
        Span::raw("  ")
    };
    let id = truncate(item.id(), ID_WIDTH);
    let left = vec![
        Span::styled(" ", base),
        marker,
        Span::styled(format!("{index} "), theme.dim.patch(base)),
        Span::styled(
            format!("{id:<width$} ", width = ID_WIDTH),
            theme.status_model.patch(base),
        ),
    ];
    let chip = chip_spans(app, &chip(item, &app.decisions_panel));
    let chip = chip
        .into_iter()
        .map(|span| Span::styled(span.content, span.style.patch(base)))
        .collect::<Vec<_>>();
    let right_text = format!("  {:>4} ", age(item));
    let right_width = spans_width(&chip) + width_of(&right_text);
    let room = width.saturating_sub(spans_width(&left) + right_width + 1);
    let full_question = item.question();
    let question = truncate(&full_question, room);
    let cut = question != full_question;
    let mut spans = left;
    spans.push(Span::styled(question, base));
    let used = spans_width(&spans);
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used + right_width)),
        base,
    ));
    spans.extend(chip);
    spans.push(Span::styled(right_text, theme.dim.patch(base)));
    (Line::from(spans), cut)
}

fn group_line(app: &App, group: &Group<'_>, width: usize) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            truncate(&group.label, width.saturating_sub(10)),
            app.theme.status_muted.add_modifier(Modifier::BOLD),
        ),
    ];
    if group.root {
        spans.push(Span::styled(" \u{b7} root", app.theme.dim));
    }
    Line::from(spans)
}

fn editor_prefix(kind: EditorKind, asker: &str) -> String {
    match kind {
        EditorKind::Own => "\u{270e} my answer: ".into(),
        EditorKind::Clarify => format!("? ask {asker}: "),
        EditorKind::Note => "note: ".into(),
    }
}

fn editor_line(
    app: &App,
    editor: &Editor,
    asker: &str,
    indent: usize,
    width: usize,
) -> (Line<'static>, usize) {
    let prefix = editor_prefix(editor.kind, asker);
    let room = width.saturating_sub(indent + width_of(&prefix) + 1).max(4);
    let before = &editor.text[..editor.cursor];
    let after = &editor.text[editor.cursor..];
    let mut shown_before = before.to_owned();
    while width_of(&shown_before) > room.saturating_sub(1) {
        let mut chars = shown_before.chars();
        chars.next();
        shown_before = chars.as_str().to_owned();
    }
    let clipped = shown_before.len() < before.len();
    if clipped {
        let mut chars = shown_before.chars();
        chars.next();
        shown_before = format!("\u{2026}{}", chars.as_str());
    }
    let cursor = indent + width_of(&prefix) + width_of(&shown_before);
    let shown_after = truncate(after, room.saturating_sub(width_of(&shown_before)));
    let style = match editor.kind {
        EditorKind::Clarify => app.theme.context_warn,
        _ => app.theme.status_model,
    };
    (
        Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(prefix, style),
            Span::raw(shown_before),
            Span::raw(shown_after),
        ]),
        cursor,
    )
}

struct Detail {
    lines: Vec<Line<'static>>,
    cursor: Option<(usize, usize)>,
}

fn detail_lines(app: &App, item: &Item<'_>, width: usize, indent: usize, full: bool) -> Detail {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let pad = " ".repeat(indent);
    let text_width = width.saturating_sub(indent + 1);
    let push_wrapped = |lines: &mut Vec<Line<'static>>, text: &str, style: Style| {
        for line in wrap(text, text_width) {
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(line, style),
            ]));
        }
    };
    let (asker, decision) = match item {
        Item::Decision(decision) => (decision.asker.name.clone(), Some(*decision)),
        Item::Confirm(child) => (child.name.clone(), None),
    };
    if full {
        push_wrapped(
            &mut lines,
            &item.question(),
            Style::default().add_modifier(Modifier::BOLD),
        );
    }
    if let Some(decision) = decision {
        if let Some(context) = &decision.context {
            push_wrapped(&mut lines, context, theme.status_muted);
        }
        if let Some(asked) = &decision.clarified {
            push_wrapped(&mut lines, &format!("you asked: {asked}"), theme.dim);
        }
        if let DecisionStatus::Clarifying { question, .. } = &decision.status {
            push_wrapped(
                &mut lines,
                &format!("waiting for {asker} to clarify: {question}"),
                theme.dim,
            );
        }
    }
    let drafted_option = match item {
        Item::Decision(decision) => match &decision.draft {
            Some(Draft {
                answer: Answer::Pick { option, .. },
                ..
            }) => Some(*option),
            _ => None,
        },
        Item::Confirm(child) => app
            .decisions_panel
            .confirms
            .get(&child.id)
            .map(|allow| usize::from(!*allow)),
    };
    for (index, label) in item.options().iter().enumerate() {
        let picked = drafted_option == Some(index);
        let label_style = if picked {
            theme.context_ok.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut label_lines = wrap(label, text_width.saturating_sub(5)).into_iter();
        let mut spans = vec![
            Span::raw(pad.clone()),
            Span::styled(if picked { "\u{25cf} " } else { "  " }, theme.context_ok),
            Span::styled(
                mush_runtime::decisions::letter(index).to_string(),
                theme.status_model.add_modifier(Modifier::BOLD),
            ),
            Span::styled(") ", theme.dim),
            Span::styled(label_lines.next().unwrap_or_default(), label_style),
        ];
        for line in label_lines {
            lines.push(Line::from(std::mem::take(&mut spans)));
            spans = vec![
                Span::raw(format!("{pad}     ")),
                Span::styled(line, label_style),
            ];
        }
        if decision.is_some_and(|decision| decision.lean == Some(index)) {
            let lean = "   \u{25c2} lean";
            if spans_width(&spans) + width_of(lean) >= width {
                lines.push(Line::from(std::mem::take(&mut spans)));
                spans = vec![Span::raw(format!("{pad}  "))];
            }
            spans.push(Span::styled(lean, theme.context_warn));
        }
        lines.push(Line::from(spans));
        if let Some(detail) = decision
            .and_then(|decision| decision.options.get(index))
            .and_then(|option| option.detail.as_deref())
        {
            for line in wrap(detail, text_width.saturating_sub(5)) {
                lines.push(Line::from(vec![
                    Span::raw(format!("{pad}     ")),
                    Span::styled(line, theme.dim),
                ]));
            }
        }
        if let Some(option) = decision.and_then(|decision| decision.options.get(index)) {
            let sides = [
                ("+", &option.pros, theme.context_ok),
                ("\u{2212}", &option.cons, theme.context_danger),
            ];
            for (marker, points, style) in sides {
                for point in points {
                    for (row, line) in wrap(point, text_width.saturating_sub(7))
                        .into_iter()
                        .enumerate()
                    {
                        let marker = if row == 0 { marker } else { " " };
                        lines.push(Line::from(vec![
                            Span::raw(format!("{pad}     ")),
                            Span::styled(format!("{marker} "), style),
                            Span::styled(line, theme.status_muted),
                        ]));
                    }
                }
            }
        }
    }
    let mut cursor = None;
    match (&app.decisions_panel.editor, decision) {
        (Some(editor), Some(_)) => {
            let (line, column) = editor_line(app, editor, &asker, indent, width);
            cursor = Some((lines.len(), column));
            lines.push(line);
            lines.push(Line::from(vec![
                Span::raw(format!("{pad}  ")),
                Span::styled("enter", theme.status_model),
                Span::styled(" keep  ", theme.dim),
                Span::styled("esc", theme.status_model),
                Span::styled(" cancel", theme.dim),
            ]));
        }
        (None, Some(decision)) => {
            match &decision.draft {
                Some(Draft {
                    answer: Answer::Own { text },
                    ..
                }) => push_wrapped(
                    &mut lines,
                    &format!("\u{270e} my answer: {text}"),
                    theme.status_model,
                ),
                Some(Draft {
                    answer: Answer::Clarify { question },
                    ..
                }) => push_wrapped(
                    &mut lines,
                    &format!("? ask {asker}: {question}"),
                    theme.context_warn,
                ),
                Some(Draft {
                    answer:
                        Answer::Pick {
                            note: Some(note), ..
                        },
                    ..
                }) => push_wrapped(&mut lines, &format!("note: {note}"), theme.status_muted),
                _ => {}
            }
            let action = |key: &'static str, label: &'static str| {
                [
                    Span::styled(key, theme.status_model.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {label}   "), theme.status_muted),
                ]
            };
            let write_label = if drafted_option.is_some() {
                "add a note"
            } else {
                "write my own"
            };
            let mut actions = vec![
                action("w", write_label),
                action("?", "ask for clarification"),
                action("l", "later"),
            ];
            if decision.draft.is_some() {
                actions.push(action("x", "clear"));
            }
            let mut spans = vec![Span::raw(pad.clone())];
            for pair in actions {
                if spans.len() > 1 && spans_width(&spans) + spans_width(&pair) > width + 2 {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                    spans.push(Span::raw(pad.clone()));
                }
                spans.extend(pair);
            }
            lines.push(Line::from(spans));
            for reference in &decision.refs {
                for (row, line) in wrap(reference, text_width.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                {
                    let arrow = if row == 0 { "\u{2197} " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::raw(pad.clone()),
                        Span::styled(format!("{arrow}{line}"), theme.link),
                    ]));
                }
            }
        }

        _ => {}
    }
    Detail { lines, cursor }
}

fn hints(app: &App, drafted: usize, overflow: bool) -> Line<'static> {
    let theme = &app.theme;
    let panel = &app.decisions_panel;
    let pairs: Vec<(String, &str)> = if panel.editor.is_some() {
        vec![("enter".into(), "keep"), ("esc".into(), "cancel")]
    } else if panel.review {
        vec![("enter".into(), "send"), ("esc".into(), "back to the list")]
    } else {
        let mut pairs = vec![("j/k".into(), "move")];
        if overflow {
            pairs.push(("J/K".into(), "scroll"));
        }
        pairs.extend([
            ("a-i".into(), "pick"),
            ("w".into(), "write"),
            ("?".into(), "clarify"),
            ("l".into(), "later"),
            ("A".into(), "accept leans"),
            ("S".into(), if drafted > 0 { "send" } else { "review" }),
            ("esc".into(), "close, drafts kept"),
        ]);
        pairs
    };
    let mut spans = vec![Span::raw(" ")];
    for (index, (key, label)) in pairs.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let label = if key == "S" && drafted > 0 {
            format!(" {label} {drafted}")
        } else {
            format!(" {label}")
        };
        spans.push(Span::styled(key, theme.status_model));
        spans.push(Span::styled(label, theme.dim));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn shrink_hints(line: Line<'static>, width: usize) -> Line<'static> {
    if spans_width(&line.spans) <= width.saturating_sub(4) {
        return line;
    }
    let mut spans = line.spans;
    while spans.len() > 3 && spans_width(&spans) > width.saturating_sub(4) {
        spans.truncate(spans.len().saturating_sub(4));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

#[derive(Debug, Clone, Copy, Default)]
struct Shown {
    cursor: Option<(u16, u16)>,
    overflow: bool,
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
    if area.width < 24 || area.height < 6 {
        return None;
    }
    Clear.render(area, buf);
    let theme = &app.theme;
    let groups = groups(&app.decisions, &app.children);
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let selected = selected_key(app, &groups);
    let shown = if groups.is_empty() {
        Line::from(Span::styled("  nothing is waiting on you", theme.dim)).render(
            Rect {
                y: inner.y + 1,
                height: 1,
                ..inner
            },
            buf,
        );
        Shown::default()
    } else if inner.width >= WIDE {
        render_wide(app, &groups, selected.as_ref(), inner, buf)
    } else {
        render_narrow(app, &groups, selected.as_ref(), inner, buf)
    };
    let (open, blocking) = crate::decisions::counts(app);
    let drafted = outgoing(app).count();
    let mut counts = vec![
        Span::raw(" "),
        Span::styled(
            format!("{open} open"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" \u{b7} ", theme.dim),
        Span::styled(format!("{drafted} drafted"), theme.context_ok),
    ];
    if blocking > 0 {
        counts.push(Span::styled(" \u{b7} ", theme.dim));
        counts.push(Span::styled(
            format!("{blocking} blocking"),
            theme.context_danger.add_modifier(Modifier::BOLD),
        ));
    }
    counts.push(Span::raw(" "));
    let bottom = match &app.decisions_panel.notice {
        Some(notice) => Line::from(vec![
            Span::raw(" "),
            Span::styled(notice.clone(), theme.context_warn),
            Span::raw(" "),
        ]),
        None => shrink_hints(hints(app, drafted, shown.overflow), usize::from(area.width)),
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme.input_border)
        .title(Line::from(Span::styled(
            " decisions ",
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title(Line::from(counts).right_aligned())
        .title_bottom(bottom.right_aligned())
        .render(area, buf);
    if app.decisions_panel.review && !groups.is_empty() {
        render_review(app, &groups, inner, buf);
        return None;
    }
    shown.cursor
}

fn render_lines(lines: &[Line<'static>], area: Rect, offset: usize, buf: &mut Buffer) {
    for (row, line) in lines
        .iter()
        .skip(offset)
        .take(usize::from(area.height))
        .enumerate()
    {
        let y = area.y + u16::try_from(row).unwrap_or(u16::MAX);
        line.clone().render(
            Rect {
                y,
                height: 1,
                ..area
            },
            buf,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Gap,
    Header,
    Item,
    Detail,
}

type Listing = Vec<(Row, Line<'static>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    offset: usize,
    body: usize,
    above: bool,
    below: bool,
}

impl Window {
    fn whole(total: usize, height: usize) -> Self {
        Self {
            offset: 0,
            body: total.min(height),
            above: false,
            below: false,
        }
    }

    fn around(rows: &[(Row, Line<'static>)], height: usize, focus: (usize, usize)) -> Self {
        let (start, end) = focus;
        let want = if end + 1 < height {
            0
        } else {
            (end + 3).saturating_sub(height).min(start)
        };
        Self::at(rows, height, want)
    }

    fn within(
        rows: &[(Row, Line<'static>)],
        height: usize,
        focus: (usize, usize),
        scroll: usize,
    ) -> Option<(Self, usize)> {
        let (start, end) = focus;
        if Self::around(rows, height, focus).screen_row(end).is_some() {
            return None;
        }
        let deepest = (0..=end - start)
            .find(|depth| {
                Self::at(rows, height, start + depth)
                    .screen_row(end)
                    .is_some()
            })
            .unwrap_or(end - start);
        let depth = scroll.min(deepest);
        Some((Self::at(rows, height, start + depth), depth))
    }

    fn at(rows: &[(Row, Line<'static>)], height: usize, want: usize) -> Self {
        let total = rows.len();
        if total <= height || height < 3 {
            return Self::whole(total, height);
        }
        let offset = want.min(total + 1 - height);
        let above = offset > 0;
        let mut body = height - usize::from(above);
        let below = offset + body < total;
        if below {
            body -= 1;
            if body > 1 && rows[offset + body - 1].0 == Row::Header {
                body -= 1;
            }
        }
        Self {
            offset,
            body,
            above,
            below,
        }
    }

    fn screen_row(self, row: usize) -> Option<usize> {
        let visible = row
            .checked_sub(self.offset)
            .filter(|row| *row < self.body)?;
        Some(visible + usize::from(self.above))
    }
}

fn more_line(arrow: char, hidden: usize, style: Style) -> Line<'static> {
    let text = match hidden {
        0 => format!("  {arrow} more"),
        _ => format!("  {arrow} {hidden} more"),
    };
    Line::from(Span::styled(text, style))
}

fn render_window(
    rows: &[(Row, Line<'static>)],
    area: Rect,
    window: Window,
    style: Style,
    buf: &mut Buffer,
) {
    let items =
        |rows: &[(Row, Line<'static>)]| rows.iter().filter(|(row, _)| *row == Row::Item).count();
    let mut lines = Vec::with_capacity(usize::from(area.height));
    if window.above {
        lines.push(more_line('\u{2191}', items(&rows[..window.offset]), style));
    }
    let shown = &rows[window.offset..window.offset + window.body];
    lines.extend(shown.iter().map(|(_, line)| line.clone()));
    if window.below {
        lines.resize(usize::from(area.height) - 1, Line::default());
        lines.push(more_line(
            '\u{2193}',
            items(&rows[window.offset + window.body..]),
            style,
        ));
    }
    render_lines(&lines, area, 0, buf);
}

fn to_screen(area: Rect, window: Window, cursor: Option<(usize, usize)>) -> Option<(u16, u16)> {
    let (row, column) = cursor?;
    let row = window.screen_row(row)?;
    Some((
        area.x
            + u16::try_from(column)
                .unwrap_or(0)
                .min(area.width.saturating_sub(1)),
        area.y + u16::try_from(row).unwrap_or(0),
    ))
}

fn render_narrow(
    app: &App,
    groups: &[Group<'_>],
    selected: Option<&ItemKey>,
    area: Rect,
    buf: &mut Buffer,
) -> Shown {
    let width = usize::from(area.width);
    let mut rows: Listing = vec![(Row::Gap, Line::default())];
    let mut focus = (0, 0);
    let mut cursor = None;
    let mut index = 0;
    for group in groups {
        rows.push((Row::Header, group_line(app, group, width)));
        for item in &group.items {
            index += 1;
            let is_selected = selected == Some(&item.key());
            let start = rows.len();
            let (row, cut) = item_row(app, item, index, is_selected, is_selected, width);
            rows.push((Row::Item, row));
            if is_selected {
                let detail = detail_lines(app, item, width, DETAIL_INDENT, cut);
                cursor = detail
                    .cursor
                    .map(|(row, column)| (rows.len() + row, column));
                rows.extend(detail.lines.into_iter().map(|line| (Row::Detail, line)));
                focus = (start, rows.len() - 1);
            }
        }
    }
    let height = usize::from(area.height);
    let panel = &app.decisions_panel;
    let (window, overflow) = match Window::within(&rows, height, focus, panel.detail_scroll.get()) {
        Some((window, depth)) => {
            panel.detail_scroll.set(depth);
            panel.detail_page.set(window.body);
            (window, true)
        }
        None => {
            panel.detail_scroll.set(0);
            (Window::around(&rows, height, focus), false)
        }
    };
    render_window(&rows, area, window, app.theme.scroll_indicator, buf);
    Shown {
        cursor: to_screen(area, window, cursor),
        overflow,
    }
}

fn render_wide(
    app: &App,
    groups: &[Group<'_>],
    selected: Option<&ItemKey>,
    area: Rect,
    buf: &mut Buffer,
) -> Shown {
    let left_width = (u32::from(area.width) * 48 / 100).clamp(50, 64) as u16;
    let left = Rect {
        width: left_width,
        ..area
    };
    let divider_x = area.x + left_width;
    let right = Rect {
        x: divider_x + 1,
        width: area.width.saturating_sub(left_width + 1),
        ..area
    };
    for y in area.y..area.bottom() {
        if let Some(cell) = buf.cell_mut((divider_x, y)) {
            cell.set_symbol("\u{2502}")
                .set_style(app.theme.input_border);
        }
    }
    let width = usize::from(left.width);
    let mut rows: Listing = vec![(Row::Gap, Line::default())];
    let mut focus = (0, 0);
    let mut chosen = None;
    let mut index = 0;
    for group in groups {
        rows.push((Row::Header, group_line(app, group, width)));
        for item in &group.items {
            index += 1;
            let is_selected = selected == Some(&item.key());
            if is_selected {
                focus = (rows.len(), rows.len());
                chosen = Some(*item);
            }
            rows.push((
                Row::Item,
                item_row(app, item, index, is_selected, false, width).0,
            ));
        }
    }
    let window = Window::around(&rows, usize::from(left.height), focus);
    render_window(&rows, left, window, app.theme.scroll_indicator, buf);
    let Some(item) = chosen else {
        return Shown::default();
    };
    let right_width = usize::from(right.width);
    let theme = &app.theme;
    let (who, asked) = match &item {
        Item::Decision(decision) => (
            decision.asker.label(),
            format!("asked {} ago", decision.asked_at.age_display()),
        ),
        Item::Confirm(child) => (child.name.clone(), "waiting now".into()),
    };
    let header_left = vec![
        Span::raw(" "),
        Span::styled(
            truncate(&who, right_width.saturating_sub(30)),
            theme.status_muted.add_modifier(Modifier::BOLD),
        ),
        Span::styled(" \u{b7} ", theme.dim),
        Span::styled(item.id().to_owned(), theme.status_model),
    ];
    let header_right = vec![Span::styled(format!("{asked} "), theme.dim)];
    let used = spans_width(&header_left);
    let gap = right_width.saturating_sub(used + spans_width(&header_right));
    let mut header = pad(header_left, used + gap, Style::default());
    header.extend(header_right);
    let mut detail_rows: Listing = [Line::default(), Line::from(header), Line::default()]
        .into_iter()
        .map(|line| (Row::Detail, line))
        .collect();
    let detail = detail_lines(app, &item, right_width, 1, true);
    let base = detail_rows.len();
    detail_rows.extend(detail.lines.into_iter().map(|line| (Row::Detail, line)));
    let panel = &app.decisions_panel;
    let window = Window::at(
        &detail_rows,
        usize::from(right.height),
        panel.detail_scroll.get(),
    );
    panel.detail_scroll.set(window.offset);
    panel.detail_page.set(window.body);
    render_window(&detail_rows, right, window, theme.scroll_indicator, buf);
    let cursor = detail.cursor.map(|(row, column)| (base + row, column));
    Shown {
        cursor: to_screen(right, window, cursor),
        overflow: window.above || window.below,
    }
}

fn recipient_state(app: &App, child: Option<&str>, confirming: bool) -> &'static str {
    let Some(child) = child else {
        return if app.is_busy() {
            "busy, it gets this at its next step"
        } else {
            "idle, this wakes it"
        };
    };
    match app.children.iter().find(|known| known.id == child) {
        None => "no longer running, its parent gets this",
        Some(known) if known.status == mush_runtime::ChildStatus::Deleted => {
            "no longer running, its parent gets this"
        }
        Some(_) if confirming => "waiting on you, this unblocks it",
        Some(known) if known.awaiting_confirmation.is_some() => "still waiting on its tool call",
        Some(known) if known.paused.is_some() => "paused, it gets this when it resumes",
        Some(known) if !known.status.settled() => "busy, it gets this at its next step",
        Some(_) => "idle, this wakes it",
    }
}

fn answer_spans(app: &App, item: &Item<'_>) -> Vec<Span<'static>> {
    let theme = &app.theme;
    match item {
        Item::Confirm(child) => {
            let allow = app.decisions_panel.confirms.get(&child.id).copied();
            vec![Span::styled(
                match allow {
                    Some(true) => "allow",
                    Some(false) => "deny",
                    None => "",
                },
                if allow == Some(false) {
                    theme.context_danger
                } else {
                    theme.context_ok
                },
            )]
        }
        Item::Decision(decision) => match decision.draft.as_ref().map(|draft| &draft.answer) {
            Some(Answer::Pick {
                option,
                label,
                note,
            }) => {
                let mut spans = vec![Span::styled(
                    format!("{}) {label}", mush_runtime::decisions::letter(*option)),
                    theme.context_ok,
                )];
                if decision.lean == Some(*option) {
                    spans.push(Span::styled("  lean", theme.context_warn));
                }
                if let Some(note) = note {
                    spans.push(Span::styled(format!("  note: {note}"), theme.status_muted));
                }
                spans
            }
            Some(Answer::Own { text }) => {
                vec![Span::styled(format!("\"{text}\""), theme.status_model)]
            }
            Some(Answer::Clarify { question }) => vec![
                Span::styled("asks: ", theme.context_warn.add_modifier(Modifier::BOLD)),
                Span::styled(format!("\"{question}\""), theme.status_model),
            ],
            _ => Vec::new(),
        },
    }
}

fn render_review(app: &App, groups: &[Group<'_>], area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let width = area.width.saturating_sub(4).min(84);
    let inner_width = usize::from(width.saturating_sub(2));
    let sendable = |item: &Item<'_>| match item {
        Item::Confirm(child) => app.decisions_panel.confirms.contains_key(&child.id),
        Item::Decision(decision) => decision
            .draft
            .as_ref()
            .is_some_and(|draft| draft.answer.sendable()),
    };
    let mut lines: Listing = vec![(Row::Gap, Line::default())];
    let mut total = 0;
    let mut index = 0;
    let mut still_open = Vec::new();
    for group in groups {
        let child = group.items.iter().find_map(|item| match item {
            Item::Decision(decision) => decision.asker.child.clone(),
            Item::Confirm(child) => Some(child.id.clone()),
        });
        let mut rows = Vec::new();
        let mut confirming = false;
        for item in &group.items {
            index += 1;
            if !sendable(item) {
                let open = match item {
                    Item::Decision(decision) => decision.waiting_on_user(),
                    Item::Confirm(_) => true,
                };
                if open {
                    still_open.push((index, item.id().to_owned(), item.blocking()));
                }
                continue;
            }
            total += 1;
            confirming |= matches!(item, Item::Confirm(_));
            let mut spans = vec![
                Span::raw("   "),
                Span::styled(format!("{index} "), theme.dim),
                Span::styled(
                    format!(
                        "{:<width$} ",
                        truncate(item.id(), ID_WIDTH),
                        width = ID_WIDTH
                    ),
                    theme.status_model,
                ),
            ];
            spans.extend(answer_spans(app, item));
            rows.push((Row::Item, Line::from(truncate_spans(spans, inner_width))));
        }
        if rows.is_empty() {
            continue;
        }
        let mut header = vec![
            Span::raw(" "),
            Span::styled(
                truncate(&group.label, inner_width.saturating_sub(40)),
                theme.status_muted.add_modifier(Modifier::BOLD),
            ),
        ];
        if group.root {
            header.push(Span::styled(" \u{b7} root", theme.dim));
        }
        let state = recipient_state(
            app,
            if group.root { None } else { child.as_deref() },
            confirming,
        );
        let gap = inner_width.saturating_sub(spans_width(&header) + width_of(state) + 1);
        header.push(Span::raw(" ".repeat(gap)));
        header.push(Span::styled(state, theme.dim));
        lines.push((Row::Header, Line::from(header)));
        lines.extend(rows);
    }
    if !still_open.is_empty() {
        lines.push((Row::Gap, Line::default()));
        let mut spans = vec![Span::styled(" still open: ", theme.dim)];
        for (position, (number, id, blocking)) in still_open.iter().enumerate() {
            if position > 0 {
                spans.push(Span::styled(", ", theme.dim));
            }
            let style = if *blocking {
                theme.context_danger.add_modifier(Modifier::BOLD)
            } else {
                theme.dim
            };
            spans.push(Span::styled(format!("{number} {id}"), style));
            if *blocking {
                spans.push(Span::styled(" (blocking)", theme.dim));
            }
        }
        lines.push((Row::Gap, Line::from(spans)));
    }
    lines.push((Row::Gap, Line::default()));
    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let scroll = &app.decisions_panel.review_scroll;
    let window = Window::at(&lines, usize::from(height.saturating_sub(2)), scroll.get());
    scroll.set(window.offset);
    let mut keys = vec![Span::raw(" ")];
    if window.above || window.below {
        keys.push(Span::styled("j/k", theme.status_model));
        keys.push(Span::styled(" scroll  ", theme.dim));
    }
    keys.extend([
        Span::styled("enter", theme.status_model),
        Span::styled(" send  ", theme.dim),
        Span::styled("esc", theme.status_model),
        Span::styled(" back to the list ", theme.dim),
    ]);
    Clear.render(rect, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.status_model)
        .title(Line::from(Span::styled(
            format!(" send {total} answer{} ", if total == 1 { "" } else { "s" }),
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title_bottom(Line::from(keys).right_aligned());
    let inner = block.inner(rect);
    block.render(rect, buf);
    render_window(&lines, inner, window, theme.scroll_indicator, buf);
}

#[cfg(test)]
mod tests;
