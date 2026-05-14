//! lightweight markdown to ratatui text renderer
//!
//! handles the common patterns in LLM output: headings, bold, italic,
//! inline code, code blocks, lists, and horizontal rules. not a full
//! markdown parser, just enough for readable agent output.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use std::sync::LazyLock;
use tabled::builder::Builder;
use tabled::settings::Width;
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

#[cfg(test)]
thread_local! {
    static RENDER_CALLS: Cell<usize> = const { Cell::new(0) };
    static PARSE_INLINE_CALLS: Cell<usize> = const { Cell::new(0) };
    static HIGHLIGHT_CODE_BLOCK_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// render a markdown string to styled ratatui Text
pub fn render(source: &str, theme: &Theme) -> Text<'static> {
    render_with_width(source, theme, None)
}

/// render markdown with an optional target width for width-aware blocks.
pub fn render_with_width(source: &str, theme: &Theme, width: Option<usize>) -> Text<'static> {
    #[cfg(test)]
    RENDER_CALLS.with(|calls| calls.set(calls.get() + 1));

    if source.is_empty() {
        return Text::default();
    }
    if is_plain_text_document(source) {
        return render_plain_text(source);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_block_lines: Vec<String> = Vec::new();

    let source_lines: Vec<&str> = source.lines().collect();
    let mut idx = 0;
    while idx < source_lines.len() {
        let raw_line = source_lines[idx];
        if raw_line.starts_with("```") {
            if in_code_block {
                // end code block - highlight and emit
                let highlighted = render_code_block(&code_block_lines, &code_block_lang, theme);
                lines.extend(highlighted);
                code_block_lines.clear();
                code_block_lang.clear();
                in_code_block = false;
            } else {
                code_block_lang = raw_line.trim_start_matches('`').trim().to_string();
                in_code_block = true;
            }
            idx += 1;
            continue;
        }

        if in_code_block {
            code_block_lines.push(raw_line.to_string());
            idx += 1;
            continue;
        }

        // table: `| ... |` header row, `|---|---|` separator, then `| ... |` body rows
        if let Some(consumed) = try_render_table(&source_lines[idx..], theme, &mut lines, width) {
            idx += consumed;
            continue;
        }

        // headings
        if let Some(rest) = raw_line.strip_prefix("### ") {
            lines.push(Line::styled(rest.to_string(), theme.heading_h3));
            idx += 1;
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix("## ") {
            lines.push(Line::styled(rest.to_string(), theme.heading));
            idx += 1;
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix("# ") {
            lines.push(Line::styled(
                rest.to_string(),
                theme.heading.add_modifier(Modifier::UNDERLINED),
            ));
            idx += 1;
            continue;
        }

        // horizontal rule
        if raw_line == "---" || raw_line == "***" || raw_line == "___" {
            lines.push(Line::styled("─".repeat(40), theme.horizontal_rule));
            idx += 1;
            continue;
        }

        // list items
        if let Some(rest) = raw_line
            .strip_prefix("- ")
            .or_else(|| raw_line.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled("• ", theme.list_bullet)];
            spans.extend(render_inline_spans(rest, theme));
            lines.push(Line::from(spans));
            idx += 1;
            continue;
        }

        // numbered list items
        if let Some((prefix, rest)) = numbered_list_item(raw_line) {
            let mut spans = vec![Span::styled(format!("{prefix} "), theme.list_bullet)];
            spans.extend(render_inline_spans(rest, theme));
            lines.push(Line::from(spans));
            idx += 1;
            continue;
        }

        // regular paragraph with inline formatting
        if raw_line.is_empty() {
            lines.push(Line::raw(""));
        } else {
            lines.push(Line::from(render_inline_spans(raw_line, theme)));
        }
        idx += 1;
    }

    // close any unclosed code block
    if in_code_block {
        let highlighted = render_code_block(&code_block_lines, &code_block_lang, theme);
        lines.extend(highlighted);
    }

    Text::from(lines)
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
            && !line.trim_start().starts_with('|')
            && line != "---"
            && line != "***"
            && line != "___"
            && numbered_list_item(line).is_none())
}

fn needs_inline_parsing(text: &str) -> bool {
    text.as_bytes()
        .iter()
        .any(|byte| matches!(*byte, b'`' | b'*' | b'_'))
}

fn numbered_list_item(line: &str) -> Option<(&str, &str)> {
    let dot_pos = line.find(". ")?;
    if dot_pos > 3 || !line[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((&line[..=dot_pos], &line[dot_pos + 2..]))
}

/// parse a `| a | b | c |` row into trimmed cell strings. returns `None`
/// if the line doesn't look like a table row (no surrounding pipes, no
/// inner pipes). the outer pipes are stripped before splitting so cells
/// never contain the delimiter itself
fn parse_table_row(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|')?.strip_suffix('|')?;
    if !inner.contains('|') {
        return None;
    }
    Some(inner.split('|').map(str::trim).collect())
}

/// a separator row uses dashes (with optional colons for alignment)
/// between the pipes. every cell must be a dash/colon run, and there
/// must be at least one dash to disambiguate from header rows that
/// contain only colons
fn is_table_separator_row(line: &str) -> bool {
    let Some(cells) = parse_table_row(line) else {
        return false;
    };
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|cell| {
        !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':') && cell.contains('-')
    })
}

/// try to recognise and render a gfm table starting at `lines[0]`.
/// returns the number of source lines consumed, or `None` if the
/// prefix doesn't form a valid table (missing separator row, ragged
/// cell counts, etc)
fn try_render_table(
    source: &[&str],
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
    width: Option<usize>,
) -> Option<usize> {
    let consumed = table_source_line_count(source)?;
    let records = pulldown_table_rows(&source[..consumed].join("\n"))?;
    let table = render_records_with_tabled(records, width);
    out.extend(table.lines().map(|line| styled_table_line(line, theme)));

    Some(consumed)
}

fn table_source_line_count(source: &[&str]) -> Option<usize> {
    parse_table_row(source.first()?)?;
    let separator = source.get(1)?;
    if !is_table_separator_row(separator) {
        return None;
    }

    let mut idx = 2;
    while idx < source.len() && parse_table_row(source[idx]).is_some() {
        idx += 1;
    }
    Some(idx)
}

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
    let mut builder = Builder::new();
    for record in records {
        builder.push_record(record);
    }
    let mut table = builder.build();
    table.with(TableStyle::sharp());
    if let Some(width) = width {
        table.with(Width::wrap(width));
    }
    table.to_string()
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
    let source_lines: Vec<&str> = source.lines().collect();
    let mut lines = Vec::new();
    try_render_table(&source_lines, &Theme::default(), &mut lines, None)?;
    Some(
        lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn render_code_block(code_lines: &[String], lang: &str, theme: &Theme) -> Vec<Line<'static>> {
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
    let mut lines = Vec::with_capacity(code_lines.len());
    for code_line in code_lines {
        lines.push(Line::raw(format!("  {code_line}")));
    }
    lines
}

/// highlight a code block using syntect
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
    let mut lines = Vec::with_capacity(code_lines.len());
    let mut line_with_nl = String::new();

    for code_line in code_lines {
        // append newline so syntect can track state across lines
        line_with_nl.clear();
        line_with_nl.push_str(code_line);
        line_with_nl.push('\n');
        match h.highlight_line(&line_with_nl, ps) {
            Ok(ranges) => {
                let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
                for (style, text) in ranges {
                    let text = text.trim_end_matches('\n').to_string();
                    if text.is_empty() {
                        continue;
                    }
                    spans.push(Span::styled(text, syntect_to_style(style)));
                }
                lines.push(Line::from(spans));
            }
            Err(_) => {
                lines.push(Line::styled(format!("  {code_line}"), ui_theme.code_block));
            }
        }
    }

    lines
}

/// convert syntect highlighting style to ratatui style
fn syntect_to_style(style: highlighting::Style) -> Style {
    let mut s = Style::default();
    if style.foreground.a > 0 {
        s = s.fg(Color::Rgb(
            style.foreground.r,
            style.foreground.g,
            style.foreground.b,
        ));
    }
    // skip background (let terminal bg show through)
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

/// parse inline markdown: **bold**, *italic*, `code`, ***bold italic***
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

    #[test]
    fn plain_text() {
        let text = render("hello world", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "hello world");
    }

    #[test]
    fn headings() {
        let text = render("# heading 1\n## heading 2\n### heading 3", &t());
        assert_eq!(text.lines.len(), 3);
        assert_eq!(text.lines[0].to_string(), "heading 1");
        assert_eq!(text.lines[1].to_string(), "heading 2");
        assert_eq!(text.lines[2].to_string(), "heading 3");
    }

    #[test]
    fn bold_text() {
        let text = render("some **bold** text", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "some bold text");
        // the middle span should be bold
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
    }

    #[test]
    fn code_block() {
        let text = render("```rust\nfn main() {}\n```", &t());
        assert_eq!(text.lines.len(), 1);
        // content should be indented with syntax highlighting applied
        let content = text.lines[0].to_string();
        assert!(content.contains("fn main()"), "got: {content}");
    }

    #[test]
    fn code_block_has_colour() {
        let text = render("```rust\nlet x = 42;\n```", &t());
        assert_eq!(text.lines.len(), 1);
        // should have multiple spans (indentation + highlighted tokens)
        assert!(
            text.lines[0].spans.len() > 1,
            "expected syntax highlighting spans, got: {:?}",
            text.lines[0].spans
        );
    }

    #[test]
    fn code_block_unknown_lang() {
        let text = render("```xyz\nhello world\n```", &t());
        assert_eq!(text.lines.len(), 1);
        assert!(text.lines[0].to_string().contains("hello world"));
    }

    #[test]
    fn plain_code_block_skips_syntax_highlighting() {
        reset_highlight_code_block_call_count();

        let text = render("```\nhello world\n```", &t());

        assert_eq!(text.lines.len(), 1);
        assert!(text.lines[0].to_string().contains("hello world"));
        assert_eq!(highlight_code_block_call_count(), 0);
    }

    #[test]
    fn unknown_language_code_block_skips_syntax_highlighting() {
        reset_highlight_code_block_call_count();

        let text = render("```xyz\nhello world\n```", &t());

        assert_eq!(text.lines.len(), 1);
        assert!(text.lines[0].to_string().contains("hello world"));
        assert_eq!(highlight_code_block_call_count(), 0);
    }

    #[test]
    fn large_known_language_code_block_skips_syntax_highlighting() {
        reset_highlight_code_block_call_count();
        let source = format!("```rust\n{}```", "let x = 42;\n".repeat(80));

        let text = render(&source, &t());

        assert_eq!(text.lines.len(), 80);
        assert!(text.lines[0].to_string().contains("let x = 42;"));
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
        assert_eq!(text.lines.len(), 3);
        assert!(text.lines[1].to_string().contains("─"));
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

        assert_eq!(text.lines.len(), 3);
        assert_eq!(parse_inline_call_count(), 1);
    }

    #[test]
    fn mixed_content() {
        let text = render("# title\n\nsome **bold** and `code`\n\n- a list item", &t());
        assert!(text.lines.len() >= 4);
    }

    #[test]
    fn table_renders_as_aligned_grid() {
        // L325: markdown tables should render as a readable box-drawing
        // grid so LLM output with tables doesn't appear as raw pipes
        let source = "\
| Name  | Age |
|-------|-----|
| Alice | 30  |
| Bob   | 25  |";
        let text = render(source, &t());
        let rendered: Vec<String> = text.lines.iter().map(Line::to_string).collect();
        // 6 lines: top border, header, separator, row1, row2, bottom border
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

        // columns should be aligned: header and data rows equal width
        assert_eq!(rendered[1].chars().count(), rendered[3].chars().count());
        assert_eq!(rendered[3].chars().count(), rendered[4].chars().count());
    }

    #[test]
    fn table_in_mixed_content() {
        // a table appearing between prose should render as a grid and
        // not disturb the surrounding paragraphs
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
        // 5 grid lines (top, header, sep, row, bottom)
        assert!(
            rendered.iter().filter(|l| l.starts_with('│')).count() == 2,
            "expected 2 grid content rows, got: {rendered:?}"
        );
    }

    #[test]
    fn incomplete_table_falls_back_to_plain() {
        // a lone pipe line without a separator row is not a table and
        // should render as regular prose, not a broken grid
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
        let text = render_with_width(source, &t(), Some(34));

        for line in &text.lines {
            let width = unicode_width::UnicodeWidthStr::width(line.to_string().as_str());
            assert!(width <= 34, "line exceeded width 34: {line:?}");
        }
        assert!(
            text.lines.len() > 5,
            "wrapped table should use extra physical rows: {:?}",
            text.lines
        );
    }

    #[test]
    fn markdown_uses_pulldown_cmark_for_tables() {
        assert!(pulldown_table_rows("| a | b |\n|---|---|\n| c | d |").is_some());
    }
}
