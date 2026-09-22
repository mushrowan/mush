use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use std::sync::LazyLock;

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
    for (event, range) in Parser::new_ext(&source, options).into_offset_iter() {
        renderer.handle(event, &source[range]);
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
    in_table: bool,
    link_stack: Vec<LinkCapture>,
    code_highlighting: CodeHighlighting,
}

struct LinkCapture {
    destination: String,
    text: String,
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
            in_table: false,
            link_stack: Vec::new(),
            code_highlighting,
        }
    }

    fn handle(&mut self, event: Event<'_>, source: &str) {
        if self.in_table {
            if matches!(event, Event::End(TagEnd::Table)) {
                self.in_table = false;
            }
            return;
        }
        if self.handle_code_block(&event) {
            return;
        }
        if matches!(event, Event::Start(Tag::Table(_))) {
            self.separate();
            self.lines.extend(render_source_table(source, self.theme));
            self.in_table = true;
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

fn render_source_table(source: &str, theme: &Theme) -> Vec<Line<'static>> {
    source
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 1 {
                Line::styled(line.to_string(), theme.horizontal_rule)
            } else {
                source_table_line(line, theme, index == 0)
            }
        })
        .collect()
}

fn source_table_line(line: &str, theme: &Theme, header: bool) -> Line<'static> {
    let content_style = if header { theme.bold } else { Style::default() };
    let mut spans = Vec::new();
    let mut content = String::new();
    let mut backslashes = 0;
    let mut code = false;
    for ch in line.chars() {
        let escaped = backslashes % 2 == 1;
        if ch == '|' && !escaped && !code {
            if !content.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut content), content_style));
            }
            spans.push(Span::styled("|", theme.horizontal_rule));
        } else {
            content.push(ch);
        }
        if ch == '`' && !escaped {
            code = !code;
        }
        if ch == '\\' {
            backslashes += 1;
        } else {
            backslashes = 0;
        }
    }
    if !content.is_empty() {
        spans.push(Span::styled(content, content_style));
    }
    Line::from(spans)
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

pub(crate) const CODE_BLOCK_INDENT: &str = "  ";

fn render_plain_code_block(code_lines: &[String]) -> Vec<Line<'static>> {
    let lines: Vec<Line<'static>> = code_lines
        .iter()
        .map(|code_line| {
            Line::from(vec![
                Span::raw(CODE_BLOCK_INDENT),
                Span::styled(code_line.clone(), Style::default()),
            ])
        })
        .collect();
    if lines.is_empty() {
        vec![Line::from(CODE_BLOCK_INDENT)]
    } else {
        lines
    }
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
    let mut lines = Vec::with_capacity(code_lines.len());
    let mut line_with_nl = String::new();

    for code_line in code_lines {
        line_with_nl.clear();
        line_with_nl.push_str(code_line);
        line_with_nl.push('\n');
        match h.highlight_line(&line_with_nl, ps) {
            Ok(ranges) => {
                let mut spans: Vec<Span<'static>> = vec![Span::raw(CODE_BLOCK_INDENT)];
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
                lines.push(Line::from(vec![
                    Span::raw(CODE_BLOCK_INDENT),
                    Span::styled(code_line.clone(), ui_theme.code_block),
                ]));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(CODE_BLOCK_INDENT));
    }
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

#[cfg(test)]
pub(crate) fn reset_render_call_count() {
    RENDER_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn render_call_count() -> usize {
    RENDER_CALLS.with(Cell::get)
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
            rendered.iter().any(|line| line == "  nix run .#profiler"),
            "code block lines are indented by two spaces and nothing else: {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains(['┌', '└', '│'])),
            "code blocks render without a box: {rendered:?}"
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
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "  fn main() {}");
    }

    #[test]
    fn code_block_has_colour() {
        let text = render("```rust\nlet x = 42;\n```", &t());
        assert_eq!(text.lines.len(), 1);
        assert!(
            text.lines[0].spans.len() > 2,
            "expected syntax highlighting spans, got: {:?}",
            text.lines[0].spans
        );
    }

    #[test]
    fn code_block_unknown_lang() {
        let text = render("```xyz\nhello world\n```", &t());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "  hello world");
    }

    #[test]
    fn an_empty_code_block_still_takes_a_line() {
        let text = render("```\n```", &t());
        assert_eq!(text.lines.len(), 1);
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
    fn mixed_content() {
        let text = render("# title\n\nsome **bold** and `code`\n\n- a list item", &t());
        assert!(text.lines.len() >= 3);
    }

    #[test]
    fn table_keeps_its_source_characters() {
        let source = "Name | Age\n:--- | ---:\nAlice | 30";
        let text = render(source, &t());
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert_eq!(rendered, source.lines().collect::<Vec<_>>());
        assert!(rendered.iter().all(|line| !line.contains('│')));
    }

    #[test]
    fn table_colours_structure_and_header() {
        let theme = t();
        let source = "| Name | Age |\n|:-----|----:|\n| Ada  | 36  |";
        let text = render(source, &theme);

        assert_eq!(text.lines[1].style, theme.horizontal_rule);
        assert!(text.lines[0].spans.iter().any(|span| {
            span.content.contains("Name") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(
            text.lines[0]
                .spans
                .iter()
                .filter(|span| span.content == "|")
                .all(|span| { span.style == theme.horizontal_rule })
        );
    }

    #[test]
    fn table_keeps_markdown_markers_and_literal_pipes() {
        let source = "| Name | Note |\n|------|------|\n| Ada  | **bold** and `a|b` and a \\| b |";
        let text = render(source, &t());
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert_eq!(rendered, source.lines().collect::<Vec<_>>());
    }

    #[test]
    fn table_in_mixed_content() {
        let source = "here is a table:\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\ndone.";
        let text = render(source, &t());
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert!(rendered.iter().any(|line| line == "here is a table:"));
        assert!(rendered.iter().any(|line| line == "|---|---|"));
        assert!(rendered.iter().any(|line| line == "| 1 | 2 |"));
        assert!(rendered.iter().any(|line| line == "done."));
    }

    #[test]
    fn incomplete_table_falls_back_to_plain() {
        let source = "| just | one | line |";
        let text = render(source, &t());
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert_eq!(rendered, [source]);
    }

    #[test]
    fn narrow_tables_keep_markdown_syntax() {
        let source = "| Name | Notes |\n|------|-------|\n| Ada  | this is a long table cell |";
        let text = render_with_width(source, &t(), Some(20));
        let rendered = text.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        assert_eq!(rendered, source.lines().collect::<Vec<_>>());
    }
}
