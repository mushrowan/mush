use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::buffer::CellWidth;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use std::sync::LazyLock;
use tabled::builder::Builder;
use tabled::settings::Width;
use tabled::settings::peaker::Priority;
use tabled::settings::style::Style as TableStyle;

use crate::theme::Theme;

#[cfg(test)]
use std::cell::Cell;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

const MAX_HIGHLIGHTED_CODE_BLOCK_LINES: usize = 64;

const HORIZONTAL_RULE_WIDTH: usize = 80;

#[cfg(test)]
thread_local! {
    static RENDER_CALLS: Cell<usize> = const { Cell::new(0) };
    static PARSE_INLINE_CALLS: Cell<usize> = const { Cell::new(0) };
    static HIGHLIGHT_CODE_BLOCK_CALLS: Cell<usize> = const { Cell::new(0) };
}

pub fn render(source: &str, theme: &Theme) -> Text<'static> {
    render_with_width(source, theme, None)
}

pub fn render_with_width(source: &str, theme: &Theme, width: Option<usize>) -> Text<'static> {
    render_with_options(source, theme, width, CodeHighlighting::Enabled)
}

pub(crate) fn render_streaming_with_width(
    source: &str,
    theme: &Theme,
    width: Option<usize>,
) -> Text<'static> {
    render_with_options(source, theme, width, CodeHighlighting::Disabled)
}

fn render_with_options(
    source: &str,
    theme: &Theme,
    width: Option<usize>,
    code_highlighting: CodeHighlighting,
) -> Text<'static> {
    #[cfg(test)]
    RENDER_CALLS.with(|calls| calls.set(calls.get() + 1));

    if source.is_empty() {
        return Text::default();
    }
    if is_plain_text_document(source) {
        return render_plain_text(source);
    }

    Text::from(render_pulldown(source, theme, width, code_highlighting))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeHighlighting {
    Enabled,
    Disabled,
}

fn render_pulldown(
    source: &str,
    theme: &Theme,
    width: Option<usize>,
    code_highlighting: CodeHighlighting,
) -> Vec<Line<'static>> {
    let source = normalise_thematic_breaks(source);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let mut renderer = MarkdownRenderer::new(theme, width, code_highlighting);
    for event in Parser::new_ext(&source, options) {
        renderer.handle(event);
    }
    renderer.finish()
}

fn normalise_thematic_breaks(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            if matches!(line.trim(), "---" | "***" | "___") {
                "\n---\n".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct MarkdownRenderer<'a> {
    theme: &'a Theme,
    width: Option<usize>,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>,
    pending_marker: Option<String>,
    quote_depth: usize,
    code_block: Option<(String, String)>,
    table: Option<TableCapture>,
    link_stack: Vec<LinkCapture>,
    code_highlighting: CodeHighlighting,
}

struct LinkCapture {
    destination: String,
    text: String,
}

#[derive(Default)]
struct TableCapture {
    rows: Vec<Vec<String>>,
    current_row: Option<Vec<String>>,
    current_cell: String,
    in_cell: bool,
    in_head: bool,
    head_row: Option<Vec<String>>,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(theme: &'a Theme, width: Option<usize>, code_highlighting: CodeHighlighting) -> Self {
        Self {
            theme,
            width,
            lines: Vec::new(),
            current: Vec::new(),
            style_stack: vec![Style::default()],
            list_stack: Vec::new(),
            pending_marker: None,
            quote_depth: 0,
            code_block: None,
            table: None,
            link_stack: Vec::new(),
            code_highlighting,
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        if self.handle_code_block(&event) || self.handle_table(&event) {
            return;
        }

        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => self.push_inline_code(&code),
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.push_current_line(),
            Event::Rule => {
                self.separate();
                let width = self.width.unwrap_or(HORIZONTAL_RULE_WIDTH);
                self.lines.push(Line::styled(
                    "─".repeat(width.min(HORIZONTAL_RULE_WIDTH)),
                    self.theme.horizontal_rule,
                ));
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.list_stack.is_empty() {
                    self.separate();
                }
            }
            Tag::Heading { level, .. } => {
                self.separate();
                self.push_style(heading_style(level, self.theme));
                if level as usize >= HeadingLevel::H3 as usize {
                    let hashes = "#".repeat(level as usize);
                    self.current
                        .push(Span::styled(format!("{hashes} "), self.current_style()));
                }
            }
            Tag::Strong => self.push_style(self.current_style().add_modifier(Modifier::BOLD)),
            Tag::Emphasis => self.push_style(self.current_style().add_modifier(Modifier::ITALIC)),
            Tag::Strikethrough => {
                self.push_style(self.current_style().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::BlockQuote(_) => {
                self.separate();
                self.quote_depth += 1;
                self.push_style(self.theme.dim.add_modifier(Modifier::ITALIC));
            }
            Tag::List(start) => {
                if self.list_stack.is_empty() {
                    self.separate();
                } else {
                    self.push_current_line();
                }
                self.list_stack.push(start);
            }
            Tag::Item => {
                let marker = match self.list_stack.last_mut().and_then(Option::as_mut) {
                    Some(next) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    None => "- ".into(),
                };
                let depth = self.list_stack.len().saturating_sub(1);
                self.pending_marker = Some(format!("{}{marker}", "  ".repeat(depth)));
            }
            Tag::CodeBlock(CodeBlockKind::Fenced(info)) => {
                self.separate();
                let lang = info.split_whitespace().next().unwrap_or("").to_string();
                self.code_block = Some((lang, String::new()));
            }
            Tag::CodeBlock(CodeBlockKind::Indented) => {}
            Tag::Table(_) => {
                self.separate();
                self.table = Some(TableCapture::default());
            }
            Tag::Link { dest_url, .. } => self.link_stack.push(LinkCapture {
                destination: dest_url.to_string(),
                text: String::new(),
            }),
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Item => self.push_current_line(),
            TagEnd::Heading(_) => {
                self.push_current_line();
                self.pop_style();
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::BlockQuote(_) => {
                self.push_current_line();
                self.pop_style();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code_block.take() {
                    let lines = text.lines().map(str::to_string).collect::<Vec<_>>();
                    self.lines.extend(render_code_block(
                        &lines,
                        &lang,
                        self.theme,
                        self.code_highlighting,
                    ));
                }
            }
            TagEnd::Link => {
                if let Some(link) = self.link_stack.pop() {
                    self.push_link_destination(&link);
                }
            }
            _ => {}
        }
    }

    fn handle_code_block(&mut self, event: &Event<'_>) -> bool {
        if let Some((_, text)) = &mut self.code_block
            && let Event::Text(chunk) = event
        {
            text.push_str(chunk);
            return true;
        }
        false
    }

    fn handle_table(&mut self, event: &Event<'_>) -> bool {
        let Some(table) = &mut self.table else {
            return false;
        };

        match event {
            Event::Start(Tag::Table(_)) => true,
            Event::Start(Tag::TableHead) => {
                table.in_head = true;
                table.current_row = Some(Vec::new());
                true
            }
            Event::Start(Tag::TableRow) => {
                table.current_row = Some(Vec::new());
                true
            }
            Event::Start(Tag::TableCell) => {
                table.current_cell.clear();
                table.in_cell = true;
                true
            }
            Event::Start(Tag::Strong) if table.in_cell => {
                table.current_cell.push_str("**");
                true
            }
            Event::Start(Tag::Emphasis) if table.in_cell => {
                table.current_cell.push('*');
                true
            }
            Event::Text(text) if table.in_cell => {
                table.current_cell.push_str(text);
                true
            }
            Event::Code(text) if table.in_cell => {
                table.current_cell.push('`');
                table.current_cell.push_str(text);
                table.current_cell.push('`');
                true
            }
            Event::SoftBreak | Event::HardBreak if table.in_cell => {
                table.current_cell.push(' ');
                true
            }
            Event::End(TagEnd::TableCell) => {
                table.in_cell = false;
                if let Some(row) = &mut table.current_row {
                    row.push(table.current_cell.trim().to_string());
                }
                true
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(row) = table.current_row.take() {
                    if table.in_head {
                        table.head_row = Some(row);
                    } else {
                        table.rows.push(row);
                    }
                }
                true
            }
            Event::End(TagEnd::TableHead) => {
                table.in_head = false;
                if let Some(row) = table.current_row.take().or_else(|| table.head_row.take()) {
                    table.rows.insert(0, row);
                }
                true
            }
            Event::End(TagEnd::Table) => {
                let table = self.table.take().unwrap_or_default();
                if !table.rows.is_empty() {
                    let rendered = render_records_with_tabled(table.rows, self.width);
                    self.lines.extend(
                        rendered
                            .lines()
                            .map(|line| styled_table_line(line, self.theme)),
                    );
                }
                true
            }
            Event::End(TagEnd::Strong) if table.in_cell => {
                table.current_cell.push_str("**");
                true
            }
            Event::End(TagEnd::Emphasis) if table.in_cell => {
                table.current_cell.push('*');
                true
            }
            _ => true,
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some(link) = self.link_stack.last_mut() {
            link.text.push_str(text);
        }
        self.current
            .push(Span::styled(text.to_string(), self.current_style()));
    }

    fn push_inline_code(&mut self, code: &str) {
        if let Some(link) = self.link_stack.last_mut() {
            link.text.push_str(code);
        }
        self.current
            .push(Span::styled(code.to_string(), self.theme.inline_code));
    }

    fn push_link_destination(&mut self, link: &LinkCapture) {
        if link.destination.is_empty() || link.destination == link.text {
            return;
        }
        self.current.push(Span::styled(
            format!(" ({})", link.destination),
            self.theme.link,
        ));
    }

    fn separate(&mut self) {
        self.push_current_line();
        if !self.lines.is_empty() {
            self.push_blank_line();
        }
    }

    fn line_opener(&mut self) -> Option<Span<'static>> {
        if let Some(marker) = self.pending_marker.take() {
            return Some(Span::styled(marker, self.theme.list_bullet));
        }
        (!self.list_stack.is_empty()).then(|| Span::raw("  ".repeat(self.list_stack.len())))
    }

    fn push_current_line(&mut self) {
        if self.current.is_empty() && self.pending_marker.is_none() {
            return;
        }
        let mut spans = Vec::with_capacity(self.current.len() + 2);
        if self.quote_depth > 0 {
            spans.push(Span::styled("│ ".repeat(self.quote_depth), self.theme.dim));
        }
        spans.extend(self.line_opener());
        spans.append(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    fn push_blank_line(&mut self) {
        if self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            return;
        }
        self.lines.push(Line::raw(""));
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn current_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&Style::default())
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.push_current_line();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

fn heading_style(level: HeadingLevel, theme: &Theme) -> Style {
    match level {
        HeadingLevel::H1 => theme.heading.add_modifier(Modifier::UNDERLINED),
        HeadingLevel::H2 => theme.heading,
        _ => theme.heading_h3,
    }
}

fn render_plain_text(source: &str) -> Text<'static> {
    let lines = source
        .lines()
        .map(|line| Line::raw(line.to_string()))
        .collect::<Vec<_>>();
    Text::from(lines)
}

fn render_inline_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    if needs_inline_parsing(text) {
        parse_inline(text, theme)
    } else {
        vec![Span::raw(text.to_string())]
    }
}

fn is_plain_text_document(source: &str) -> bool {
    source.lines().all(is_plain_text_line)
}

fn is_plain_text_line(line: &str) -> bool {
    line.is_empty()
        || (!needs_inline_parsing(line)
            && !line.starts_with("```")
            && !line.starts_with("### ")
            && !line.starts_with("## ")
            && !line.starts_with("# ")
            && !line.starts_with("- ")
            && !line.starts_with("* ")
            && !line.starts_with("> ")
            && !line.trim_start().starts_with('|')
            && line != "---"
            && line != "***"
            && line != "___"
            && numbered_list_item(line).is_none())
}

fn needs_inline_parsing(text: &str) -> bool {
    text.as_bytes()
        .iter()
        .any(|byte| matches!(*byte, b'`' | b'*' | b'_' | b'[' | b']'))
}

fn numbered_list_item(line: &str) -> Option<(&str, &str)> {
    let dot_pos = line.find(". ")?;
    if dot_pos > 3 || !line[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((&line[..=dot_pos], &line[dot_pos + 2..]))
}

#[cfg(test)]
fn pulldown_table_rows(source: &str) -> Option<Vec<Vec<String>>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(source, options);

    let mut rows = Vec::new();
    let mut current_row: Option<Vec<String>> = None;
    let mut current_cell = String::new();
    let mut in_cell = false;
    let mut saw_table = false;
    let mut in_table_head = false;
    let mut head_row: Option<Vec<String>> = None;

    for event in parser {
        match event {
            Event::Start(Tag::Table(_)) => saw_table = true,
            Event::Start(Tag::TableHead) => {
                in_table_head = true;
                current_row = Some(Vec::new());
            }
            Event::Start(Tag::TableRow) => current_row = Some(Vec::new()),
            Event::Start(Tag::TableCell) => {
                current_cell.clear();
                in_cell = true;
            }
            Event::Start(Tag::Strong) if in_cell => current_cell.push_str("**"),
            Event::Start(Tag::Emphasis) if in_cell => current_cell.push('*'),
            Event::Text(text) if in_cell => current_cell.push_str(&text),
            Event::Code(text) if in_cell => {
                current_cell.push('`');
                current_cell.push_str(&text);
                current_cell.push('`');
            }
            Event::SoftBreak | Event::HardBreak if in_cell => current_cell.push(' '),
            Event::End(TagEnd::TableCell) => {
                in_cell = false;
                if let Some(row) = &mut current_row {
                    row.push(current_cell.trim().to_string());
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(row) = current_row.take() {
                    if in_table_head {
                        head_row = Some(row);
                    } else {
                        rows.push(row);
                    }
                }
            }
            Event::End(TagEnd::TableHead) => {
                in_table_head = false;
                if let Some(row) = current_row.take().or_else(|| head_row.take()) {
                    rows.insert(0, row);
                }
            }
            Event::End(TagEnd::Strong) if in_cell => current_cell.push_str("**"),
            Event::End(TagEnd::Emphasis) if in_cell => current_cell.push('*'),
            _ => {}
        }
    }

    (saw_table && !rows.is_empty()).then_some(rows)
}

fn render_records_with_tabled(records: Vec<Vec<String>>, width: Option<usize>) -> String {
    if width.is_some_and(|width| width < 24) {
        return render_records_as_list(records);
    }

    let Some(width) = width else {
        return render_records_with_tabled_width(&records, None);
    };

    let mut wrap_width = width;
    loop {
        let rendered = render_records_with_tabled_width(&records, Some(wrap_width));
        let rendered_width = rendered_table_cell_width(&rendered);
        if rendered_width <= width || wrap_width <= 1 {
            return rendered;
        }

        let overflow = rendered_width - width;
        wrap_width = wrap_width.saturating_sub(overflow.max(1)).max(1);
    }
}

fn render_records_with_tabled_width(records: &[Vec<String>], width: Option<usize>) -> String {
    let mut builder = Builder::new();
    for record in records {
        builder.push_record(record.iter().cloned());
    }
    let mut table = builder.build();
    table.with(TableStyle::sharp());
    if let Some(width) = width {
        table.with(
            Width::wrap(width)
                .keep_words(true)
                .priority(Priority::max(true)),
        );
    }
    table.to_string()
}

fn rendered_table_cell_width(table: &str) -> usize {
    table
        .lines()
        .map(|line| usize::from(line.cell_width()))
        .max()
        .unwrap_or(0)
}

fn render_records_as_list(records: Vec<Vec<String>>) -> String {
    let Some((header, rows)) = records.split_first() else {
        return String::new();
    };

    let mut out = String::new();
    for (row_index, row) in rows.iter().enumerate() {
        if row_index > 0 {
            out.push('\n');
        }
        for (idx, cell) in row.iter().enumerate() {
            let key = header.get(idx).filter(|key| !key.trim().is_empty());
            match key {
                Some(key) => out.push_str(&format!("- {}: {}\n", key.trim(), cell.trim())),
                None => out.push_str(&format!("- {}\n", cell.trim())),
            }
        }
    }
    out.trim_end().to_string()
}

fn styled_table_line(line: &str, theme: &Theme) -> Line<'static> {
    if is_table_border_line(line) {
        return Line::styled(line.to_string(), theme.horizontal_rule);
    }
    let mut spans = Vec::new();
    for segment in line.split_inclusive('│') {
        if segment == "│" {
            spans.push(Span::styled("│", theme.horizontal_rule));
        } else if let Some(cell) = segment.strip_suffix('│') {
            spans.extend(render_inline_spans(cell, theme));
            spans.push(Span::styled("│", theme.horizontal_rule));
        } else {
            spans.extend(render_inline_spans(segment, theme));
        }
    }
    Line::from(spans)
}

fn is_table_border_line(line: &str) -> bool {
    line.chars()
        .all(|c| matches!(c, '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' | '─'))
}

#[cfg(test)]
fn render_markdown_table_with_tabled(source: &str) -> Option<String> {
    let rows = pulldown_table_rows(source)?;
    let table = render_records_with_tabled(rows, None);
    let lines = table
        .lines()
        .map(|line| styled_table_line(line, &Theme::default()))
        .collect::<Vec<_>>();
    Some(
        lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn render_code_block(
    code_lines: &[String],
    lang: &str,
    theme: &Theme,
    code_highlighting: CodeHighlighting,
) -> Vec<Line<'static>> {
    if code_highlighting == CodeHighlighting::Disabled {
        return render_plain_code_block(code_lines);
    }

    let ps = &*SYNTAX_SET;
    let Some(syntax) = code_block_syntax(ps, lang) else {
        return render_plain_code_block(code_lines);
    };
    if code_lines.len() > MAX_HIGHLIGHTED_CODE_BLOCK_LINES {
        return render_plain_code_block(code_lines);
    }
    highlight_code_block(code_lines, syntax, ps, theme)
}

fn code_block_syntax<'a>(ps: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    (!lang.is_empty()).then_some(())?;
    ps.find_syntax_by_token(lang)
}

fn render_plain_code_block(code_lines: &[String]) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(code_lines.len() + 2);
    let width = code_block_width(code_lines, "");
    lines.push(Line::styled(
        format!("┌{}┐", "─".repeat(width + 2)),
        Style::default().fg(Color::DarkGray),
    ));
    for code_line in code_lines {
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            Span::styled(pad_code_line(code_line, width), Style::default()),
            Span::styled(" │", Style::default().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::styled(
        format!("└{}┘", "─".repeat(width + 2)),
        Style::default().fg(Color::DarkGray),
    ));
    lines
}

fn code_block_width(code_lines: &[String], lang: &str) -> usize {
    let content_width = code_lines
        .iter()
        .map(|line| usize::from(line.as_str().cell_width()))
        .max()
        .unwrap_or(0);
    content_width.max(usize::from(lang.cell_width())).max(1)
}

fn pad_code_line(line: &str, width: usize) -> String {
    let line_width = usize::from(line.cell_width());
    format!("{line}{}", " ".repeat(width.saturating_sub(line_width)))
}

fn highlight_code_block(
    code_lines: &[String],
    syntax: &SyntaxReference,
    ps: &SyntaxSet,
    ui_theme: &Theme,
) -> Vec<Line<'static>> {
    #[cfg(test)]
    HIGHLIGHT_CODE_BLOCK_CALLS.with(|calls| calls.set(calls.get() + 1));

    let theme = &THEME_SET.themes["base16-ocean.dark"];
    let mut h = HighlightLines::new(syntax, theme);
    let width = code_block_width(code_lines, &syntax.name);
    let mut lines = Vec::with_capacity(code_lines.len() + 2);
    let mut line_with_nl = String::new();

    lines.push(Line::from(vec![
        Span::styled("┌─ ", ui_theme.horizontal_rule),
        Span::styled(syntax.name.to_string(), ui_theme.inline_code),
        Span::styled(
            format!(
                " {}┐",
                "─".repeat(width.saturating_sub(usize::from(syntax.name.cell_width())))
            ),
            ui_theme.horizontal_rule,
        ),
    ]));

    for code_line in code_lines {
        line_with_nl.clear();
        line_with_nl.push_str(code_line);
        line_with_nl.push('\n');
        match h.highlight_line(&line_with_nl, ps) {
            Ok(ranges) => {
                let mut spans: Vec<Span<'static>> =
                    vec![Span::styled("│ ", ui_theme.horizontal_rule)];
                for (style, text) in ranges {
                    let text = text.trim_end_matches('\n').to_string();
                    if text.is_empty() {
                        continue;
                    }
                    spans.push(Span::styled(text, syntect_to_style(style)));
                }
                let line_width = usize::from(code_line.as_str().cell_width());
                if width > line_width {
                    spans.push(Span::raw(" ".repeat(width - line_width)));
                }
                spans.push(Span::styled(" │", ui_theme.horizontal_rule));
                lines.push(Line::from(spans));
            }
            Err(_) => {
                lines.push(Line::from(vec![
                    Span::styled("│ ", ui_theme.horizontal_rule),
                    Span::styled(pad_code_line(code_line, width), ui_theme.code_block),
                    Span::styled(" │", ui_theme.horizontal_rule),
                ]));
            }
        }
    }

    lines.push(Line::styled(
        format!("└{}┘", "─".repeat(width + 2)),
        ui_theme.horizontal_rule,
    ));

    lines
}

fn syntect_to_style(style: highlighting::Style) -> Style {
    let mut s = Style::default();
    if style.foreground.a > 0 {
        s = s.fg(Color::Rgb(
            style.foreground.r,
            style.foreground.g,
            style.foreground.b,
        ));
    }
    let fs = style.font_style;
    if fs.contains(highlighting::FontStyle::BOLD) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if fs.contains(highlighting::FontStyle::ITALIC) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if fs.contains(highlighting::FontStyle::UNDERLINE) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

fn parse_inline(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    #[cfg(test)]
    PARSE_INLINE_CALLS.with(|calls| calls.set(calls.get() + 1));

    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut offset = 0;

    while offset < text.len() {
        let rest = &text[offset..];
        let Some(ch) = rest.chars().next() else {
            break;
        };

        match ch {
            '`' => {
                let code_rest = &rest[ch.len_utf8()..];
                if let Some(end) = code_rest.find('`') {
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    spans.push(Span::styled(
                        code_rest[..end].to_string(),
                        theme.inline_code,
                    ));
                    offset += ch.len_utf8() + end + ch.len_utf8();
                } else {
                    buf.push(ch);
                    offset += ch.len_utf8();
                }
            }
            '*' | '_' => {
                let run_len = rest
                    .chars()
                    .take_while(|candidate| *candidate == ch)
                    .take(3)
                    .count();
                let marker_width = ch.len_utf8();

                let (delimiter_len, style) = if run_len >= 3 {
                    (3, theme.bold.patch(theme.italic))
                } else if run_len >= 2 {
                    (2, theme.bold)
                } else {
                    (1, theme.italic)
                };

                let delimiter = ch.to_string().repeat(delimiter_len);
                let inner_rest = &rest[delimiter_len * marker_width..];
                if let Some(end) = inner_rest.find(&delimiter) {
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    spans.push(Span::styled(inner_rest[..end].to_string(), style));
                    offset += delimiter_len * marker_width + end + delimiter_len * marker_width;
                } else {
                    buf.push_str(&delimiter);
                    offset += delimiter_len * marker_width;
                }
            }
            _ => {
                buf.push(ch);
                offset += ch.len_utf8();
            }
        }
    }

    if !buf.is_empty() {
        spans.push(Span::raw(buf));
    }

    if spans.is_empty() {
        spans.push(Span::raw(""));
    }

    spans
}

#[cfg(test)]
pub(crate) fn reset_render_call_count() {
    RENDER_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn render_call_count() -> usize {
    RENDER_CALLS.with(Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_parse_inline_call_count() {
    PARSE_INLINE_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn parse_inline_call_count() -> usize {
    PARSE_INLINE_CALLS.with(Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_highlight_code_block_call_count() {
    HIGHLIGHT_CODE_BLOCK_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn highlight_code_block_call_count() -> usize {
    HIGHLIGHT_CODE_BLOCK_CALLS.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Theme {
        Theme::default()
    }

    fn rendered_width(line: &Line<'_>) -> u16 {
        line.to_string().as_str().cell_width()
    }

    #[test]
    fn a_heading_does_not_colour_the_text_after_it() {
        let text = render("## a heading\n\nplain prose after it", &t());
        let prose = text
            .lines
            .iter()
            .find(|line| line.to_string().contains("prose"))
            .expect("the paragraph should render");

        assert!(
            prose
                .spans
                .iter()
                .all(|span| span.style == Style::default()),
            "heading style leaked: {:?}",
            prose.spans
        );
    }

    fn rendered(source: &str) -> Vec<String> {
        render(source, &t())
            .lines
            .iter()
            .map(Line::to_string)
            .collect()
    }

    #[test]
    fn a_blank_line_separates_two_blocks() {
        assert_eq!(
            rendered("first para\n\nsecond para\n\n## a heading\n\nthird para"),
            [
                "first para",
                "",
                "second para",
                "",
                "a heading",
                "",
                "third para"
            ]
        );
    }

    #[test]
    fn what_follows_a_list_marker_lines_up_under_it() {
        assert_eq!(
            rendered("- one\n\n  more about one\n\n- two"),
            ["- one", "  more about one", "- two"]
        );
    }

    #[test]
    fn a_nested_list_is_indented_under_its_parent() {
        assert_eq!(
            rendered("- one\n  - nested\n- two"),
            ["- one", "  - nested", "- two"]
        );
    }

    #[test]
    fn an_ordered_list_counts_from_where_it_starts() {
        assert_eq!(rendered("4. four\n5. five"), ["4. four", "5. five"]);
    }

    #[test]
    fn a_quote_wears_a_border() {
        assert_eq!(rendered("> quoted\n\nafter"), ["│ quoted", "", "after"]);
    }

    #[test]
    fn a_deeper_heading_keeps_its_hashes() {
        assert_eq!(rendered("### deep"), ["### deep"]);
    }

    #[test]
    fn plain_text() {
        let text = render("hello world", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "hello world");
    }

    #[test]
    fn headings() {
        assert_eq!(
            rendered("# heading 1\n## heading 2\n### heading 3"),
            ["heading 1", "", "heading 2", "", "### heading 3"]
        );
    }

    #[test]
    fn bold_text() {
        let text = render("some **bold** text", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "some bold text");
        assert_eq!(text.lines[0].spans.len(), 3);
        assert!(
            text.lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn unmatched_bold_is_literal() {
        let text = render("some **bold", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "some **bold");
    }

    #[test]
    fn unmatched_bold_italic_is_literal() {
        let text = render("some ***bold", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "some ***bold");
    }

    #[test]
    fn italic_text() {
        let text = render("some *italic* text", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].spans.len(), 3);
        assert!(
            text.lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn inline_code() {
        let text = render("use `cargo build`", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "use cargo build");
        assert_eq!(text.lines[0].spans[1].style, t().inline_code);
    }

    #[test]
    fn terminal_markdown_spacing_and_code_blocks() {
        let source = "Current wrappers:\n\n```sh\nnix run .#profiler\n```\n\nenables:\nMUSH_TRACE=true\nMUSH_PROFILE_STARTUP=true";
        let text = render(source, &t());
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert!(
            rendered
                .windows(2)
                .any(|lines| lines == ["Current wrappers:", ""]),
            "paragraphs should be separated from following blocks: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("sh")),
            "fenced code blocks should show their language label: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("nix run .#profiler")),
            "code block content should remain visible: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains('┌'))
                && rendered.iter().any(|line| line.contains('└')),
            "code blocks should render with a visible box: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line == "enables: MUSH_TRACE=true MUSH_PROFILE_STARTUP=true"),
            "soft breaks inside prose should become spaces: {rendered:?}"
        );
    }

    #[test]
    fn links_include_url_when_text_differs() {
        let text = render("read [the docs](https://example.com/docs)", &t());

        assert_eq!(
            text.lines[0].to_string(),
            "read the docs (https://example.com/docs)"
        );
    }

    #[test]
    fn code_block() {
        let text = render("```rust\nfn main() {}\n```", &t());
        assert_eq!(text.lines.len(), 3);
        let content = text.lines[1].to_string();
        assert!(content.contains("fn main()"), "got: {content}");
    }

    #[test]
    fn code_block_has_colour() {
        let text = render("```rust\nlet x = 42;\n```", &t());
        assert_eq!(text.lines.len(), 3);
        assert!(
            text.lines[1].spans.len() > 1,
            "expected syntax highlighting spans, got: {:?}",
            text.lines[1].spans
        );
    }

    #[test]
    fn code_block_unknown_lang() {
        let text = render("```xyz\nhello world\n```", &t());
        assert_eq!(text.lines.len(), 3);
        assert!(text.lines[1].to_string().contains("hello world"));
    }

    #[test]
    fn plain_code_block_skips_syntax_highlighting() {
        reset_highlight_code_block_call_count();

        let text = render("```\nhello world\n```", &t());

        assert_eq!(text.lines.len(), 3);
        assert!(text.lines[1].to_string().contains("hello world"));
        assert_eq!(highlight_code_block_call_count(), 0);
    }

    #[test]
    fn plain_code_block_uses_terminal_cell_width_for_halfwidth_dakuten() {
        use ratatui::buffer::CellWidth;

        let text = render("```\nabcｶﾞ\n```", &t());
        let border_width = text.lines[1].to_string().cell_width();
        let content_width = text.lines[2].to_string().cell_width();

        assert_eq!(content_width, border_width);
    }

    #[test]
    fn indented_markdown_is_not_treated_as_code_block() {
        let text = render("currently:\n    core/component.yml owns:Pipeline", &t());
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec!["currently:", "    core/component.yml owns:Pipeline"]
        );
        assert_eq!(text.lines[1].spans[0].style, Style::default());
    }

    #[test]
    fn unknown_language_code_block_skips_syntax_highlighting() {
        reset_highlight_code_block_call_count();

        let text = render("```xyz\nhello world\n```", &t());

        assert_eq!(text.lines.len(), 3);
        assert!(text.lines[1].to_string().contains("hello world"));
        assert_eq!(highlight_code_block_call_count(), 0);
    }

    #[test]
    fn large_known_language_code_block_skips_syntax_highlighting() {
        reset_highlight_code_block_call_count();
        let source = format!("```rust\n{}```", "let x = 42;\n".repeat(80));

        let text = render(&source, &t());

        assert_eq!(text.lines.len(), 82);
        assert!(text.lines[1].to_string().contains("let x = 42;"));
        assert_eq!(highlight_code_block_call_count(), 0);
    }

    #[test]
    fn unordered_list() {
        let text = render("- item one\n- item two", &t());
        assert_eq!(text.lines.len(), 2);
        assert!(text.lines[0].to_string().contains("item one"));
        assert!(text.lines[1].to_string().contains("item two"));
    }

    #[test]
    fn numbered_list() {
        let text = render("1. first\n2. second", &t());
        assert_eq!(text.lines.len(), 2);
        assert!(text.lines[0].to_string().contains("first"));
    }

    #[test]
    fn horizontal_rule() {
        let text = render("above\n---\nbelow", &t());
        assert_eq!(text.lines.len(), 5);
        assert!(text.lines[2].to_string().contains("─"));
    }

    #[test]
    fn empty_input() {
        let text = render("", &t());
        assert!(text.lines.is_empty());
    }

    #[test]
    fn plain_lines_skip_inline_parser() {
        reset_parse_inline_call_count();

        let text = render("alpha\nbeta\ngamma", &t());

        assert_eq!(text.lines.len(), 3);
        assert_eq!(parse_inline_call_count(), 0);
    }

    #[test]
    fn only_formatted_lines_use_inline_parser() {
        reset_parse_inline_call_count();

        let text = render("alpha\n**beta**\ngamma", &t());

        assert_eq!(text.lines.len(), 1);
        assert_eq!(parse_inline_call_count(), 0);
    }

    #[test]
    fn mixed_content() {
        let text = render("# title\n\nsome **bold** and `code`\n\n- a list item", &t());
        assert!(text.lines.len() >= 3);
    }

    #[test]
    fn table_renders_as_aligned_grid() {
        let source = "\
| Name  | Age |
|-------|-----|
| Alice | 30  |
| Bob   | 25  |";
        let text = render(source, &t());
        let rendered: Vec<String> = text.lines.iter().map(Line::to_string).collect();
        assert_eq!(
            rendered.len(),
            6,
            "expected 6 grid lines, got {}: {rendered:?}",
            rendered.len()
        );
        assert!(
            rendered[0].starts_with('┌') && rendered[0].contains('┬') && rendered[0].ends_with('┐')
        );
        assert!(
            rendered[1].contains("Name")
                && rendered[1].contains("Age")
                && rendered[1].starts_with('│')
        );
        assert!(
            rendered[2].starts_with('├') && rendered[2].contains('┼') && rendered[2].ends_with('┤')
        );
        assert!(rendered[3].contains("Alice") && rendered[3].contains("30"));
        assert!(rendered[4].contains("Bob") && rendered[4].contains("25"));
        assert!(
            rendered[5].starts_with('└') && rendered[5].contains('┴') && rendered[5].ends_with('┘')
        );

        assert_eq!(rendered[1].chars().count(), rendered[3].chars().count());
        assert_eq!(rendered[3].chars().count(), rendered[4].chars().count());
    }

    #[test]
    fn table_in_mixed_content() {
        let source = "\
here is a table:

| a | b |
|---|---|
| 1 | 2 |

done.";
        let text = render(source, &t());
        let rendered: Vec<String> = text.lines.iter().map(Line::to_string).collect();
        assert!(rendered.iter().any(|l| l.contains("here is a table:")));
        assert!(rendered.iter().any(|l| l.contains("done.")));
        assert!(
            rendered.iter().filter(|l| l.starts_with('│')).count() == 2,
            "expected 2 grid content rows, got: {rendered:?}"
        );
    }

    #[test]
    fn incomplete_table_falls_back_to_plain() {
        let source = "| just | one | line |";
        let text = render(source, &t());
        let rendered: Vec<String> = text.lines.iter().map(Line::to_string).collect();
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0], "| just | one | line |");
    }

    #[test]
    fn tabled_dependency_is_used_for_markdown_tables() {
        let source = "\
| Name | Age |
|------|-----|
| Ada  | 36  |";
        let rendered = render_markdown_table_with_tabled(source).expect("table should render");

        assert!(rendered.contains('┌'));
        assert!(rendered.contains("Ada"));
    }

    #[test]
    fn tabled_table_preserves_inline_formatting_in_cells() {
        let source = "\
| Name | Note |
|------|------|
| Ada  | **bold** |";
        let text = render(source, &t());

        let data_row = text
            .lines
            .iter()
            .find(|line| line.to_string().contains("bold"))
            .expect("table data row should render");
        assert!(data_row.spans.iter().any(|span| {
            span.content.as_ref() == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn table_wraps_to_requested_width() {
        let source = "\
| Name | Notes |
|------|-------|
| Ada  | this is a very long table cell that should wrap |";
        let text = render_with_width(source, &t(), Some(28));

        for line in &text.lines {
            let width = rendered_width(line);
            assert!(width <= 28, "line exceeded width 28: {line:?}");
        }
        assert!(
            text.lines.len() > 5,
            "wrapped table should use extra physical rows: {:?}",
            text.lines
        );
    }

    #[test]
    fn table_wraps_long_unspaced_cells_to_requested_width() {
        let source = "\
| Path | Digest |
|------|--------|
| crate | abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz |";
        let text = render_with_width(source, &t(), Some(30));

        for line in &text.lines {
            let width = rendered_width(line);
            assert!(width <= 30, "line exceeded width 30: {line:?}");
        }
    }

    #[test]
    fn table_wraps_halfwidth_dakuten_to_requested_cell_width() {
        let source = "\
| Name | Notes |
|------|-------|
| Ada  | abcｶﾞabcｶﾞabcｶﾞabcｶﾞabcｶﾞabcｶﾞabcｶﾞ |";
        let text = render_with_width(source, &t(), Some(30));

        for line in &text.lines {
            let width = rendered_width(line);
            assert!(width <= 30, "line exceeded width 30: {line:?}");
        }
    }

    #[test]
    fn narrow_table_falls_back_to_key_value_list() {
        let source = "\
| Name | Notes |
|------|-------|
| Ada  | this is a long table cell |";
        let text = render_with_width(source, &t(), Some(20));
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert_eq!(rendered[0], "- Name: Ada");
        assert_eq!(rendered[1], "- Notes: this is a long table cell");
        assert!(rendered.iter().all(|line| !line.contains('│')));
    }

    #[test]
    fn markdown_uses_pulldown_cmark_for_tables() {
        assert!(pulldown_table_rows("| a | b |\n|---|---|\n| c | d |").is_some());
    }

    #[test]
    fn imbalanced_table_keeps_first_column_content() {
        let source = "\
| what the two vars actually mean today | actually configures |
|---|---|
| `spectral_base_domain` | the standalone spectral API endpoint when studio is NOT deployed. caddy renders one site: `<spectral_domain>` -> `spectral:8000`. |
| `spectral_domain` | studio's root domain. studio UI lives at the root, with `api.` / `kratos.` / `hydra.` subdomains hanging off it. when studio is present, spectral's API also routes via this domain (studio owns the root, spectral piggybacks). |";

        for width in [60usize, 70, 80, 90, 100, 120] {
            let text = render_with_width(source, &t(), Some(width));
            let rendered = text
                .lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                rendered.contains("spectral_base_domain"),
                "first-column content was dropped at width {width}:\n{rendered}"
            );
            assert!(
                rendered.contains("spectral_domain"),
                "first-column content was dropped at width {width}:\n{rendered}"
            );
            assert!(
                rendered.contains("two vars"),
                "first-column header was dropped at width {width}:\n{rendered}"
            );
        }
    }
}
