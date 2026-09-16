use mush_ai::types::TokenCount;
#[cfg(test)]
use ratatui::buffer::Buffer;
use ratatui::buffer::CellWidth;
#[cfg(test)]
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::app::{App, ChatDetail, DisplayMessage, DisplayToolCall, MessageRole, ToolCallStatus};
use crate::widgets::shimmer::shimmer_spans;
use std::cell::Ref;

pub(crate) const MESSAGE_INDENT_LEFT: usize = 1;
pub(crate) const MESSAGE_INDENT_RIGHT: usize = 1;

#[must_use]
pub(crate) fn message_content_width(outer_width: usize) -> usize {
    outer_width.saturating_sub(MESSAGE_INDENT_LEFT + MESSAGE_INDENT_RIGHT)
}

#[must_use]
pub(crate) fn common_leading_space(text: &str) -> usize {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0)
}

#[must_use]
pub(crate) fn drop_leading_spaces(line: &str, count: usize) -> &str {
    let mut end = 0;
    for (i, c) in line.char_indices().take(count) {
        if c == ' ' {
            end = i + 1;
        } else {
            break;
        }
    }
    &line[end..]
}

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static INDENT_LINE_CALLS: Cell<usize> = const { Cell::new(0) };
    static COUNT_ESTIMATED_LINES_CALLS: Cell<usize> = const { Cell::new(0) };
    static CONTENT_HASH_CALLS: Cell<usize> = const { Cell::new(0) };
    static MESSAGE_BUILD_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_message_build_count() {
    MESSAGE_BUILD_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn message_build_count() -> usize {
    MESSAGE_BUILD_CALLS.with(Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_indent_line_call_count() {
    INDENT_LINE_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn indent_line_call_count() -> usize {
    INDENT_LINE_CALLS.with(Cell::get)
}

#[cfg(test)]
fn reset_content_hash_calls() {
    CONTENT_HASH_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
fn content_hash_call_count() -> usize {
    CONTENT_HASH_CALLS.with(Cell::get)
}
use crate::theme::Theme;

pub(crate) fn image_placeholder(index: usize, bytes: &[u8]) -> String {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok();
    let kind = reader
        .as_ref()
        .and_then(|reader| reader.format())
        .map(|format| format.extensions_str()[0])
        .unwrap_or("image");
    match reader.and_then(|reader| reader.into_dimensions().ok()) {
        Some((w, h)) => format!("[image {index}: {kind} {w}x{h}]"),
        None => format!("[image {index}: {kind}]"),
    }
}

fn streaming_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if app.stream.active {
        let dim = app.theme.dim;
        let stream_content_width = message_content_width(width as usize);

        if !app.stream.thinking.is_empty()
            && app.thinking_display != crate::app::ThinkingDisplay::Hidden
        {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(shimmer_spans(
                " thinking",
                &app.theme.shimmer,
                app.shimmer_phase(),
                dim,
            ));
            lines.push(Line::from(spans));
            let visible_thinking = app.visible_streaming_thinking();
            let strip = common_leading_space(visible_thinking);
            for text_line in visible_thinking.lines() {
                let body = drop_leading_spaces(text_line, strip);
                let styled = Line::styled(body.to_string(), app.theme.thinking);
                lines.extend(indent_line(styled, stream_content_width));
            }
            lines.push(Line::raw(""));
        }
        if !app.stream.text.is_empty() {
            let visible_text = app.visible_streaming_text();
            let md_text = render_streaming_markdown_cached(app, visible_text, stream_content_width);
            for line in md_text.lines {
                lines.extend(indent_line(line, stream_content_width));
            }
        }
        if app.stream.text.is_empty() && app.stream.thinking.is_empty() {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(shimmer_spans(
                " working",
                &app.theme.shimmer,
                app.shimmer_phase(),
                dim,
            ));
            lines.push(Line::from(spans));
        }
    }

    if app.active_tools.is_empty() && !app.stream.tool_args.is_empty() {
        let preview = truncate_line(&app.stream.tool_args, 60);
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(" building ", app.theme.dim),
            Span::styled(preview, app.theme.dim),
        ]));
    }

    if app.compacting {
        let dim = app.theme.dim;
        let mut spans = vec![Span::raw(" ")];
        spans.extend(shimmer_spans(
            " compacting…",
            &app.theme.shimmer,
            app.shimmer_phase(),
            dim,
        ));
        lines.push(Line::from(spans));
    }

    lines
}

pub(crate) struct TranscriptGuards<'a> {
    blank: Line<'static>,
    settled: Vec<Ref<'a, [Line<'static>]>>,
    streaming: Vec<Line<'static>>,
    queued: Vec<Ref<'a, [Line<'static>]>>,
}

impl TranscriptGuards<'_> {
    pub(crate) fn flatten(&self) -> Vec<&Line<'static>> {
        let mut lines = Vec::new();
        for chunk in &self.settled {
            lines.extend(chunk.iter());
            lines.push(&self.blank);
        }
        lines.extend(self.streaming.iter());
        for chunk in &self.queued {
            lines.extend(chunk.iter());
            lines.push(&self.blank);
        }
        lines
    }
}

pub(crate) fn transcript_guards(app: &App, width: u16) -> TranscriptGuards<'_> {
    let style = MessageStyle::for_app(app);
    let chunks = |queued: bool| {
        app.messages
            .iter()
            .filter(move |m| m.queued == queued)
            .map(|m| cached_message_lines(m, &style, width))
            .collect()
    };
    TranscriptGuards {
        blank: Line::raw(""),
        settled: chunks(false),
        streaming: streaming_lines(app, width),
        queued: chunks(true),
    }
}

pub fn transcript_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let style = MessageStyle::for_app(app);
    let mut lines = Vec::new();
    for msg in app.messages.iter().filter(|m| !m.queued) {
        lines.extend(message_lines(msg, &style, width));
        lines.push(Line::raw(""));
    }
    lines.extend(streaming_lines(app, width));
    for msg in app.messages.iter().filter(|m| m.queued) {
        lines.extend(message_lines(msg, &style, width));
        lines.push(Line::raw(""));
    }
    lines
}

pub fn live_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let style = MessageStyle::for_app(app);
    let live = app.messages.get(app.commit_point()..).unwrap_or_default();
    let mut lines = Vec::new();
    for msg in live.iter().filter(|m| !m.queued) {
        lines.extend(message_lines(msg, &style, width));
        lines.push(Line::raw(""));
    }
    lines.extend(streaming_lines(app, width));
    for msg in live.iter().filter(|m| m.queued) {
        lines.extend(message_lines(msg, &style, width));
        lines.push(Line::raw(""));
    }
    lines
}

pub struct MessageStyle<'a> {
    pub theme: &'a Theme,
    pub show_usage: bool,
    pub detail: ChatDetail,
}

impl<'a> MessageStyle<'a> {
    pub fn for_app(app: &'a App) -> Self {
        Self {
            theme: &app.theme,
            show_usage: app.interaction.show_usage_lines,
            detail: app.detail,
        }
    }
}

pub fn message_lines(
    msg: &DisplayMessage,
    style: &MessageStyle<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    cached_message_lines(msg, style, width).to_vec()
}

pub(crate) fn cached_message_lines<'a>(
    msg: &'a DisplayMessage,
    style: &MessageStyle<'_>,
    width: u16,
) -> Ref<'a, [Line<'static>]> {
    let fresh = msg.line_cache.borrow().as_ref().is_some_and(|cache| {
        cache.width == width && cache.show_usage == style.show_usage && cache.detail == style.detail
    });
    if !fresh {
        let lines = build_message_lines(msg, style, width);
        msg.line_cache
            .replace(Some(crate::display_types::LineCache {
                width,
                show_usage: style.show_usage,
                detail: style.detail,
                lines,
            }));
    }
    Ref::map(msg.line_cache.borrow(), |cache| {
        cache
            .as_ref()
            .map_or(&[][..], |cache| cache.lines.as_slice())
    })
}

fn build_message_lines(
    msg: &DisplayMessage,
    style: &MessageStyle<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    #[cfg(test)]
    MESSAGE_BUILD_CALLS.with(|c| c.set(c.get() + 1));

    let mut lines = Vec::new();

    if matches!(msg.role, MessageRole::System) {
        lines.push(system_label_line(style.theme, None));
    }
    if matches!(msg.role, MessageRole::Event(_)) {
        lines.push(Line::from(Span::styled(
            "non-human event",
            style.theme.system_label,
        )));
    }
    push_thinking_lines(msg, style, width, &mut lines);

    if msg.queued {
        push_queued_lines(msg, style.theme, width, &mut lines);
    } else if matches!(msg.role, MessageRole::User) {
        render_user_message_box(style.theme, msg, width, &mut lines);
    } else {
        let content_width = message_content_width(width as usize);
        let md = crate::markdown::render_with_width(&msg.content, style.theme, Some(content_width));
        for line in md.lines {
            lines.extend(indent_line(line, content_width));
        }
    }

    push_tool_box_lines(style, msg, width, &mut lines);
    push_usage_line(msg, style, &mut lines);
    lines
}

fn system_label_line(theme: &Theme, marker: Option<Span<'static>>) -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(marker);
    spans.push(Span::styled("system", theme.system_label));
    Line::from(spans)
}

const HEADER_SEPARATOR: &str = " \u{b7} ";

pub fn thinking_recap(thinking: &str) -> String {
    let mut lines = thinking.lines().map(str::trim).filter(|l| !l.is_empty());
    let source = lines
        .clone()
        .rfind(|l| is_bold_header(l) || is_markdown_heading(l))
        .or_else(|| lines.next())
        .unwrap_or_default();

    let plain: String = source
        .trim_start_matches('#')
        .trim_start()
        .chars()
        .filter(|c| !matches!(c, '*' | '`'))
        .collect();
    plain.trim_end_matches(':').trim_end().to_string()
}

fn is_bold_header(line: &str) -> bool {
    let Some(inner) = line
        .strip_prefix("**")
        .and_then(|l| l.strip_suffix(':').unwrap_or(l).strip_suffix("**"))
    else {
        return false;
    };
    !inner.is_empty() && !inner.contains('*')
}

fn is_markdown_heading(line: &str) -> bool {
    let hashes = line.len() - line.trim_start_matches('#').len();
    (1..=6).contains(&hashes)
        && line[hashes..].starts_with(' ')
        && !line[hashes..].trim().is_empty()
}

fn push_thinking_lines(
    msg: &DisplayMessage,
    style: &MessageStyle<'_>,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let theme = style.theme;
    let Some(ref thinking) = msg.thinking else {
        return;
    };
    let cw = message_content_width(width as usize);
    if !(msg.thinking_expanded || style.detail.shows_thinking()) {
        let label = "thinking";
        let prefix = usize::from(label.cell_width()) + usize::from(HEADER_SEPARATOR.cell_width());
        let recap = Line::from(vec![
            Span::styled(label, theme.dim),
            Span::styled(HEADER_SEPARATOR, theme.dim),
            Span::styled(
                crate::text::truncate_with_ellipsis(
                    &thinking_recap(thinking),
                    cw.saturating_sub(prefix).max(1),
                ),
                theme.thinking,
            ),
        ]);
        lines.extend(indent_line(recap, cw));
        return;
    }
    let strip = common_leading_space(thinking);
    for text_line in thinking.lines() {
        let body = drop_leading_spaces(text_line, strip);
        let styled = Line::styled(body.to_string(), theme.thinking);
        lines.extend(indent_line(styled, cw));
    }
}

fn push_queued_lines(
    msg: &DisplayMessage,
    theme: &Theme,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let cw = message_content_width(width as usize);
    for text_line in msg.content.lines() {
        let styled = Line::styled(text_line.to_string(), theme.dim);
        lines.extend(indent_line(styled, cw));
    }
}

fn push_usage_line(msg: &DisplayMessage, style: &MessageStyle<'_>, lines: &mut Vec<Line<'static>>) {
    if !style.show_usage || msg.is_live() {
        return;
    }
    let Some(ref usage) = msg.usage else {
        return;
    };
    let total = usage.total_tokens();
    if total == TokenCount::ZERO {
        return;
    }

    let mut parts = vec![format!(" {total}tok")];
    let total_input = usage.total_input_tokens();
    if total_input > TokenCount::ZERO {
        let reuse_pct = usage.cache_read_tokens.percent_of(total_input) as u32;
        parts.push(format!("reuse {reuse_pct}%"));
    }
    if usage.cache_write_tokens > TokenCount::ZERO {
        let write_pct = usage.cache_write_tokens.percent_of(total_input) as u32;
        parts.push(format!("write {write_pct}%"));
    }
    if let Some(c) = msg.cost {
        parts.push(format!("{c}"));
    }
    lines.push(Line::styled(parts.join(" | "), style.theme.usage));
}

const USER_PAD: usize = 2;

fn render_user_message_box(
    theme: &Theme,
    msg: &DisplayMessage,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let bg = theme.user_msg_bg;
    let w = width as usize;
    let content_width = w.saturating_sub(USER_PAD * 2).max(1);
    let blank = || Line::styled(" ".repeat(w), bg);

    lines.push(blank());
    for (index, bytes) in msg.images.iter().enumerate() {
        let placeholder = image_placeholder(index + 1, bytes);
        let pad = content_width.saturating_sub(text_cell_width(&placeholder));
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(USER_PAD), bg),
            Span::styled(placeholder, bg.patch(theme.dim)),
            Span::styled(" ".repeat(pad + USER_PAD), bg),
        ]));
    }
    for line in msg.content.lines() {
        for wrapped in wrap_text(line, content_width) {
            let pad = content_width.saturating_sub(text_cell_width(&wrapped));
            let indent = " ".repeat(USER_PAD);
            let trailing = " ".repeat(pad + USER_PAD);
            lines.push(Line::styled(format!("{indent}{wrapped}{trailing}"), bg));
        }
    }
    lines.push(blank());
}

fn truncate_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    let char_count = line.chars().count();
    if char_count <= max {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max).collect();
        format!("{truncated}…")
    }
}

fn render_streaming_markdown_cached(app: &App, source: &str, width: usize) -> Text<'static> {
    if source.is_empty() {
        return Text::default();
    }

    let full = &app.stream.text;

    if let Some((cached_width, cached_source, cached)) =
        app.render_state.stream_markdown_cache.borrow().as_ref()
        && *cached_width == width
        && cached_source == full
    {
        return truncate_text(cached.clone(), source.chars().count());
    }

    let rendered = crate::markdown::render_streaming_with_width(full, &app.theme, Some(width));
    *app.render_state.stream_markdown_cache.borrow_mut() =
        Some((width, full.to_string(), rendered.clone()));
    truncate_text(rendered, source.chars().count())
}

fn truncate_text(text: Text<'static>, max_chars: usize) -> Text<'static> {
    let total: usize = text.lines.iter().map(rendered_line_char_count).sum();
    if total <= max_chars {
        return text;
    }

    let mut remaining = max_chars;
    let mut result = Vec::new();
    for line in text.lines {
        if remaining == 0 {
            break;
        }
        let line_chars = rendered_line_char_count(&line);
        if line_chars <= remaining {
            remaining -= line_chars;
            result.push(line);
        } else {
            let mut spans = Vec::new();
            let mut left = remaining;
            for span in line.spans {
                let span_chars = span.content.chars().count();
                if span_chars <= left {
                    left -= span_chars;
                    spans.push(span);
                } else if left > 0 {
                    let truncated: String = span.content.chars().take(left).collect();
                    spans.push(Span::styled(truncated, span.style));
                    break;
                } else {
                    break;
                }
            }
            result.push(Line::from(spans));
            break;
        }
    }
    Text::from(result)
}

fn rendered_line_char_count(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

fn text_cell_width(text: &str) -> usize {
    usize::from(text.cell_width())
}

fn char_cell_width(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    let mut buf = [0; 4];
    text_cell_width(ch.encode_utf8(&mut buf))
}

fn spans_cell_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| text_cell_width(span.content.as_ref()))
        .sum()
}

fn line_cell_width(line: &Line<'_>) -> usize {
    spans_cell_width(&line.spans)
}

fn push_tool_box_lines(
    style: &MessageStyle<'_>,
    msg: &DisplayMessage,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let calls: Vec<&DisplayToolCall> = msg.tool_calls.iter().collect();

    for group in calls.chunk_by(|a, b| a.batch == b.batch) {
        render_tool_box_group(group, width, lines, style);
    }
}

fn render_tool_box_group(
    tools: &[&DisplayToolCall],
    total_width: u16,
    lines: &mut Vec<Line<'static>>,
    style: &MessageStyle<'_>,
) {
    if total_width < 8 {
        return;
    }
    for tc in tools {
        render_tool_panel(tc, total_width as usize, lines, style);
    }
}

const PANEL_PAD: usize = 2;

fn panel_content_width(width: usize) -> usize {
    width.saturating_sub(PANEL_PAD * 2).max(1)
}

fn push_panel_line(
    content: Vec<Span<'static>>,
    width: usize,
    bg: Style,
    lines: &mut Vec<Line<'static>>,
) {
    let pad = panel_content_width(width).saturating_sub(spans_cell_width(&content));
    let mut spans = vec![Span::styled(" ".repeat(PANEL_PAD), bg)];
    spans.extend(content.into_iter().map(|s| {
        let style = s.style;
        Span::styled(s.content, bg.patch(style))
    }));
    spans.push(Span::styled(" ".repeat(pad + PANEL_PAD), bg));
    lines.push(Line::from(spans));
}

fn visible_tool_output(tc: &DisplayToolCall, detail: ChatDetail) -> Option<String> {
    let output = tc.output.as_deref()?;
    if detail.shows_full_output() || tc.name == "edit" {
        return Some(output.to_string());
    }
    Some(crate::batch_output::truncate_output(output))
}

fn render_tool_panel(
    tc: &DisplayToolCall,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    style: &MessageStyle<'_>,
) {
    let theme = style.theme;
    let bg = theme.tool_panel_bg;
    let inner = panel_content_width(width);
    let (status, colour) = tool_status_label(tc, theme);

    push_panel_line(
        vec![
            Span::styled(
                tc.name.clone(),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            Span::styled(HEADER_SEPARATOR, theme.dim),
            Span::styled(status, Style::default().fg(colour)),
        ],
        width,
        bg,
        lines,
    );

    if tc.name == "ipython" {
        let ctx = ToolHighlightCtx {
            bg,
            fallback_style: theme.dim,
            lang: "py",
            theme,
        };
        if style.detail.shows_thinking() {
            for code_line in tc.summary.lines() {
                push_highlighted_box_line(code_line, inner, &ctx, lines);
            }
        } else {
            let (preview, hidden) = code_preview(&tc.summary);
            push_highlighted_box_line(preview, inner, &ctx, lines);
            if hidden > 0 {
                push_box_content_line(
                    &format!("... (+{hidden} lines, ctrl+r to expand)"),
                    inner,
                    bg,
                    theme.dim,
                    lines,
                );
            }
        }
    } else if let Some(lang) = tool_summary_language(tc) {
        push_highlighted_box_line(
            &tc.summary,
            inner,
            &ToolHighlightCtx {
                bg,
                fallback_style: theme.dim,
                lang,
                theme,
            },
            lines,
        );
    } else {
        push_box_content_line(&tc.summary, inner, bg, theme.dim, lines);
    }

    let Some(output) = visible_tool_output(tc, style.detail) else {
        return;
    };
    let output = output.as_str();
    if tc.name == "edit" {
        let lang = crate::syntax::lang_from_path(&tc.summary);
        for row in super::diff::render_diff_with_lang_mode(
            output,
            inner,
            theme,
            lang,
            super::diff::DiffLayout::Inline,
        ) {
            push_panel_line(row.spans, width, bg, lines);
        }
    } else if tc.name == "read" {
        let ctx = ReadLineCtx {
            bg,
            gutter_width: read_line_number_width(output),
            strip_indent: read_min_content_indent(output),
            lang: crate::syntax::lang_from_path(&tc.summary),
            theme,
        };
        for text_line in output.lines() {
            push_read_content_line(text_line, inner, &ctx, lines);
        }
    } else {
        let lang = tool_output_language(tc);
        for text_line in output.lines() {
            let base_style = diff_line_style(text_line, theme);
            if text_line.contains('\x1b') {
                push_ansi_box_lines(text_line, inner, bg, base_style, lines);
            } else if let Some(lang) = lang {
                push_highlighted_box_line(
                    text_line,
                    inner,
                    &ToolHighlightCtx {
                        bg,
                        fallback_style: base_style,
                        lang,
                        theme,
                    },
                    lines,
                );
            } else {
                push_box_content_line(text_line, inner, bg, base_style, lines);
            }
        }
    }
}

pub(crate) fn code_preview(code: &str) -> (&str, usize) {
    let lines: Vec<&str> = code.lines().collect();
    let low_signal = |line: &str| {
        let trimmed = line.trim();
        trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
    };
    let preview = lines
        .iter()
        .copied()
        .find(|line| !low_signal(line))
        .or_else(|| lines.iter().copied().find(|line| !line.trim().is_empty()))
        .unwrap_or("");
    (preview.trim_end(), lines.len().saturating_sub(1))
}

fn tool_summary_language(tc: &DisplayToolCall) -> Option<&'static str> {
    match tc.name.to_lowercase().replace('_', "").as_str() {
        "bash" => Some("sh"),
        "write" | "edit" | "read" => crate::syntax::lang_from_path(&tc.summary),
        _ => None,
    }
}

fn push_ansi_box_lines(
    text: &str,
    inner_width: usize,
    bg: Style,
    base_style: Style,
    lines: &mut Vec<Line<'static>>,
) {
    for spans in super::ansi::ansi_lines(text, base_style) {
        for wrapped in wrap_styled_spans(spans, inner_width, base_style) {
            push_panel_line(wrapped, inner_width + PANEL_PAD * 2, bg, lines);
        }
    }
}

fn push_fitted_box_line(
    spans: Vec<Span<'static>>,
    inner_width: usize,
    bg: Style,
    base_style: Style,
    lines: &mut Vec<Line<'static>>,
) {
    let fitted = super::diff::fit_spans(spans, inner_width, base_style);
    push_panel_line(fitted, inner_width + PANEL_PAD * 2, bg, lines);
}

fn tool_output_language(tc: &DisplayToolCall) -> Option<&'static str> {
    match tc.name.to_lowercase().replace('_', "").as_str() {
        "bash" => Some("sh"),
        "ipython" => Some("text"),
        "write" | "applypatch" => crate::syntax::lang_from_path(&tc.summary),
        "grep" | "find" | "glob" | "ls" | "repomap" | "websearch" | "webfetch" => Some("text"),
        _ => None,
    }
}

fn push_box_content_line(
    text: &str,
    inner_width: usize,
    bg: Style,
    style: Style,
    lines: &mut Vec<Line<'static>>,
) {
    for wrapped in wrap_text(text, inner_width) {
        push_panel_line(
            vec![Span::styled(wrapped, style)],
            inner_width + PANEL_PAD * 2,
            bg,
            lines,
        );
    }
}

struct ToolHighlightCtx<'a> {
    bg: Style,
    fallback_style: Style,
    lang: &'a str,
    theme: &'a Theme,
}

fn push_highlighted_box_line(
    text: &str,
    inner_width: usize,
    ctx: &ToolHighlightCtx<'_>,
    lines: &mut Vec<Line<'static>>,
) {
    if text_cell_width(text) > inner_width {
        push_box_content_line(text, inner_width, ctx.bg, ctx.fallback_style, lines);
        return;
    }

    let Some(spans) = crate::syntax::highlight_line(text, ctx.lang, ctx.theme) else {
        push_box_content_line(text, inner_width, ctx.bg, ctx.fallback_style, lines);
        return;
    };
    for wrapped in wrap_styled_spans(spans, inner_width, ctx.fallback_style) {
        push_fitted_box_line(wrapped, inner_width, ctx.bg, ctx.fallback_style, lines);
    }
}

fn wrap_styled_spans(
    spans: Vec<Span<'static>>,
    width: usize,
    _pad_style: Style,
) -> Vec<Vec<Span<'static>>> {
    if width == 0 {
        return vec![spans];
    }

    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    for span in spans {
        append_span_wrapped(span, width, &mut current, &mut current_width, &mut rows);
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

fn append_span_wrapped(
    span: Span<'static>,
    width: usize,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    rows: &mut Vec<Vec<Span<'static>>>,
) {
    let style = span.style;
    let mut chunk = String::new();
    for ch in span.content.chars() {
        let ch_width = char_cell_width(ch);
        if *current_width + ch_width > width && *current_width > 0 {
            flush_styled_chunk(&mut chunk, style, current);
            rows.push(std::mem::take(current));
            *current_width = 0;
            if ch == ' ' {
                continue;
            }
        }
        chunk.push(ch);
        *current_width += ch_width;
    }
    flush_styled_chunk(&mut chunk, style, current);
    if *current_width == width {
        rows.push(std::mem::take(current));
        *current_width = 0;
    }
}

fn flush_styled_chunk(chunk: &mut String, style: Style, current: &mut Vec<Span<'static>>) {
    if !chunk.is_empty() {
        current.push(Span::styled(std::mem::take(chunk), style));
    }
}

#[must_use]
pub(crate) fn parse_read_line(line: &str) -> Option<(u32, &str)> {
    let rest = line.strip_prefix('L')?;
    let colon = rest.find(": ")?;
    let number_str = &rest[..colon];
    if number_str.is_empty() || !number_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = number_str.parse().ok()?;
    Some((n, &rest[colon + 2..]))
}

#[must_use]
pub(crate) fn read_line_number_width(text: &str) -> usize {
    text.lines()
        .filter_map(parse_read_line)
        .map(|(n, _)| n.to_string().len())
        .max()
        .unwrap_or(0)
}

#[must_use]
pub(crate) fn read_min_content_indent(text: &str) -> usize {
    text.lines()
        .filter_map(parse_read_line)
        .filter(|(_, content)| !content.trim().is_empty())
        .map(|(_, content)| content.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0)
}

/// otherwise takes individually (clippy::too_many_arguments).
struct ReadLineCtx<'a> {
    bg: Style,
    gutter_width: usize,
    strip_indent: usize,
    lang: Option<&'a str>,
    theme: &'a Theme,
}

fn push_read_content_line(
    text: &str,
    inner_width: usize,
    ctx: &ReadLineCtx<'_>,
    lines: &mut Vec<Line<'static>>,
) {
    let Some((n, content)) = parse_read_line(text) else {
        push_box_content_line(text, inner_width, ctx.bg, ctx.theme.dim, lines);
        return;
    };

    let content = if content.len() >= ctx.strip_indent
        && content.as_bytes()[..ctx.strip_indent]
            .iter()
            .all(|b| *b == b' ')
    {
        &content[ctx.strip_indent..]
    } else {
        content
    };

    let gutter = format!("{:>w$}  ", n, w = ctx.gutter_width);
    let gutter_width = text_cell_width(&gutter);
    let remaining_width = inner_width.saturating_sub(gutter_width);

    let content_spans: Vec<Span<'static>> = ctx
        .lang
        .and_then(|l| crate::syntax::highlight_line(content, l, ctx.theme))
        .unwrap_or_else(|| vec![Span::raw(content.to_string())]);

    let content_width = spans_cell_width(&content_spans);

    if content_width > remaining_width {
        push_box_content_line(text, inner_width, ctx.bg, ctx.theme.dim, lines);
        return;
    }

    let mut spans = vec![Span::styled(gutter, ctx.theme.dim)];
    spans.extend(content_spans);
    push_panel_line(spans, inner_width + PANEL_PAD * 2, ctx.bg, lines);
}

fn indent_line<'a>(line: Line<'a>, content_width: usize) -> Vec<Line<'a>> {
    #[cfg(test)]
    INDENT_LINE_CALLS.with(|c| c.set(c.get() + 1));

    let line_style = line.style;
    if content_width == 0 {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(line.spans);
        return vec![Line::from(spans).style(line_style)];
    }
    if line_cell_width(&line) <= content_width {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(line.spans);
        return vec![Line::from(spans).style(line_style)];
    }

    let cells: Vec<(char, Style, usize)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = span.style;
            span.content.chars().map(move |ch| {
                let w = char_cell_width(ch);
                (ch, style, w)
            })
        })
        .collect();

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style, usize)> = Vec::new();
    let mut row_width: usize = 0;
    let mut last_space: Option<usize> = None;

    for (ch, style, w) in cells {
        if row_width + w > content_width && !row.is_empty() {
            if let Some(sp) = last_space {
                let tail: Vec<(char, Style, usize)> = row.drain(sp + 1..).collect();
                let _space = row.pop();
                rows.push(strip_row(&row));
                row_width = tail.iter().map(|(_, _, w)| w).sum();
                row = tail;
                last_space = row.iter().rposition(|(c, _, _)| *c == ' ');
            } else {
                rows.push(strip_row(&row));
                row.clear();
                row_width = 0;
                last_space = None;
            }
        }
        if ch == ' ' {
            last_space = Some(row.len());
        }
        row.push((ch, style, w));
        row_width += w;
    }

    if !row.is_empty() {
        rows.push(strip_row(&row));
    }

    let mut result: Vec<Line<'a>> = Vec::with_capacity(rows.len());
    for cells in rows {
        let mut spans: Vec<Span<'a>> = vec![Span::raw(" ")];
        let mut buf = String::new();
        let mut cur_style: Option<Style> = None;
        for (ch, style) in cells {
            match cur_style {
                Some(s) if s == style => buf.push(ch),
                _ => {
                    if let Some(s) = cur_style
                        && !buf.is_empty()
                    {
                        spans.push(Span::styled(std::mem::take(&mut buf), s));
                    }
                    cur_style = Some(style);
                    buf.push(ch);
                }
            }
        }
        if let Some(s) = cur_style
            && !buf.is_empty()
        {
            spans.push(Span::styled(buf, s));
        }
        result.push(Line::from(spans).style(line_style));
    }

    result
}

fn strip_row(row: &[(char, Style, usize)]) -> Vec<(char, Style)> {
    let end = row
        .iter()
        .rposition(|(c, _, _)| *c != ' ')
        .map_or(0, |p| p + 1);
    row[..end].iter().map(|(c, s, _)| (*c, *s)).collect()
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    if text_cell_width(text) <= width {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width: usize = 0;

    for word in text.split(' ') {
        let word_width = text_cell_width(word);

        if word_width > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let chunks = split_word_by_cell_width(word, width);
            let last_index = chunks.len().saturating_sub(1);
            for (index, chunk) in chunks.into_iter().enumerate() {
                if index == last_index {
                    current_width = text_cell_width(&chunk);
                    current = chunk;
                } else {
                    lines.push(chunk);
                }
            }
        } else if current.is_empty() {
            current = word.to_string();
            current_width = word_width;
        } else if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
            current_width = word_width;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_word_by_cell_width(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in word.chars() {
        let ch_width = char_cell_width(ch);
        if current_width + ch_width > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn tool_status_label(tc: &DisplayToolCall, theme: &Theme) -> (&'static str, Color) {
    match tc.status {
        ToolCallStatus::Done => ("done", theme.tool_done.fg.unwrap_or(Color::Green)),
        ToolCallStatus::Error => ("error", theme.tool_error.fg.unwrap_or(Color::Red)),
        ToolCallStatus::Running => ("running", theme.tool_running.fg.unwrap_or(Color::Cyan)),
    }
}

fn diff_line_style(line: &str, theme: &Theme) -> Style {
    if line.starts_with("+ ") {
        theme.diff_added
    } else if line.starts_with("- ") {
        theme.diff_removed
    } else {
        theme.dim
    }
}

#[cfg(test)]
fn write_lines_to_buf(lines: &[Line<'_>], buf: &mut Buffer, area: Rect, start_y: u16) -> u16 {
    let mut written = 0u16;
    for line in lines {
        let y = start_y + written;
        if y >= area.bottom() {
            break;
        }
        buf.set_line(area.x, y, line, area.width);
        written += 1;
    }
    written
}

#[cfg(test)]
mod tests;
