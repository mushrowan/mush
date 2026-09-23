use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{
    HEADER_SEPARATOR, MessageStyle, char_cell_width, code_preview, spans_cell_width,
    tool_status_label, wrap_styled_spans,
};
use crate::app::{ChatDetail, DisplayToolCall, ToolCallStatus};
use crate::theme::Theme;
use crate::widgets::working_icon::WORKING_ICON_FRAMES;

const CODE_GUTTER: &str = "\u{2570}\u{2500} ";
const OUTPUT_GUTTER: &str = " \u{203a} ";
const INDENT: &str = "   ";

pub(super) fn render_cell(
    tc: &DisplayToolCall,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    style: &MessageStyle<'_>,
) {
    let theme = style.theme;
    let cell = tc.cell.as_ref();
    let code = tc.summary.trim_end();
    let output = CellOutputView::new(tc, style.detail, theme);

    lines.push(top_line(tc, code, &output, width, style));
    if !style.detail.shows_thinking() {
        return;
    }

    let has_code = !code.is_empty();
    if has_code {
        for (index, raw) in code.lines().enumerate() {
            let gutter = if index == 0 { CODE_GUTTER } else { INDENT };
            let spans = crate::syntax::highlight_line(raw, "py", theme)
                .unwrap_or_else(|| vec![Span::styled(raw.to_string(), theme.code_block)]);
            push_gutter(lines, gutter, theme.dim, spans, width);
        }
    }

    let mut gutter = OutputGutter::new(has_code);
    for block in &output.blocks {
        for text_line in block.text.lines() {
            let text = if text_line.is_empty() { " " } else { text_line };
            let gutter_text = gutter.next(lines);
            push_gutter(
                lines,
                gutter_text,
                theme.dim,
                vec![Span::styled(text.to_string(), block.style)],
                width,
            );
        }
    }
    if output.blocks.is_empty() {
        let placeholder = match tc.status {
            ToolCallStatus::Running => Some("waiting for output..."),
            _ if cell.is_none_or(|cell| cell.diffs.is_empty() && cell.images == 0) => {
                Some("no output")
            }
            _ => None,
        };
        if let Some(placeholder) = placeholder {
            let gutter_text = gutter.next(lines);
            push_gutter(
                lines,
                gutter_text,
                theme.dim,
                vec![Span::styled(placeholder.to_string(), theme.dim)],
                width,
            );
        }
    }

    if let Some(cell) = cell {
        render_diffs(cell, width, lines, style, &mut gutter);
        render_sent_messages(cell, width, lines, style, &mut gutter);
        if cell.images > 0 {
            let plural = if cell.images == 1 { "" } else { "s" };
            let gutter_text = gutter.next(lines);
            push_gutter(
                lines,
                gutter_text,
                theme.dim,
                vec![Span::styled(
                    format!("{} image{plural} sent to the model", cell.images),
                    theme.dim,
                )],
                width,
            );
        }
    }

    if let Some(done) = &tc.bash_done {
        let (label, colour) = super::bash_done_label(done, theme);
        let gutter_text = gutter.next(lines);
        push_gutter(
            lines,
            gutter_text,
            theme.dim,
            vec![Span::styled(
                format!(
                    "{label}{HEADER_SEPARATOR}pid {}",
                    done.pid
                        .map_or_else(|| "?".to_string(), |pid| pid.to_string())
                ),
                Style::default().fg(colour),
            )],
            width,
        );
    }
}

fn top_line(
    tc: &DisplayToolCall,
    code: &str,
    output: &CellOutputView,
    width: usize,
    style: &MessageStyle<'_>,
) -> Line<'static> {
    let theme = style.theme;
    let (_, colour) = tool_status_label(tc, theme);
    let marker = match tc.status {
        ToolCallStatus::Done => "\u{2713}",
        ToolCallStatus::Error => "\u{2717}",
        ToolCallStatus::Running => WORKING_ICON_FRAMES[style.pulse % WORKING_ICON_FRAMES.len()],
    };
    let mut parts: Vec<Span<'static>> = vec![Span::styled("python", theme.dim)];
    let (preview, _) = code_preview(code);
    if !preview.trim().is_empty() {
        parts.push(Span::styled(preview.trim().to_string(), theme.dim));
    }
    let input_lines = code.lines().filter(|line| !line.trim().is_empty()).count();
    let mut counts = Vec::new();
    if input_lines > 0 {
        counts.push(format!("\u{2191} {input_lines}"));
    }
    if output.line_count > 0 {
        counts.push(format!("\u{2193} {}", output.line_count));
    }
    if !counts.is_empty() {
        parts.push(Span::styled(
            format!("{} lines", counts.join(" ")),
            theme.dim,
        ));
    }
    if let Some(duration) = tc
        .cell
        .as_ref()
        .map(|cell| format_duration(cell.duration_ms))
    {
        parts.push(Span::styled(duration, theme.dim));
    }
    if let Some(error) = tc.cell.as_ref().and_then(|cell| cell.error.as_ref()) {
        parts.push(Span::styled(error.ename.clone(), theme.tool_error));
    }

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(marker.to_string(), Style::default().fg(colour)),
        Span::raw(" "),
    ];
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(HEADER_SEPARATOR, theme.dim));
        }
        spans.push(part);
    }
    truncate_spans(spans, width)
}

struct OutputBlock {
    text: String,
    style: Style,
}

struct CellOutputView {
    blocks: Vec<OutputBlock>,
    line_count: usize,
}

impl CellOutputView {
    fn new(tc: &DisplayToolCall, detail: ChatDetail, theme: &Theme) -> Self {
        let mut blocks = Vec::new();
        let mut raw_lines = 0;
        let mut push = |text: &str, style: Style, blocks: &mut Vec<OutputBlock>| {
            let text = text.trim_end_matches('\n');
            if text.trim().is_empty() {
                return;
            }
            raw_lines += text.lines().count();
            let shown = if detail.shows_full_output() {
                text.to_string()
            } else {
                crate::batch_output::truncate_output(text)
            };
            blocks.push(OutputBlock { text: shown, style });
        };
        match &tc.cell {
            Some(cell) => {
                push(&cell.stdout, theme.tool_output, &mut blocks);
                push(&cell.stderr, theme.dim, &mut blocks);
                if let Some(result) = &cell.result
                    && !is_edit_confirmation(result, cell)
                    && !is_send_receipt(result, cell)
                {
                    push(result, theme.tool_output, &mut blocks);
                }
                if let Some(error) = &cell.error {
                    let traceback = if error.traceback.is_empty() {
                        format!("{}: {}", error.ename, error.evalue)
                    } else {
                        error.traceback.join("\n")
                    };
                    push(&traceback, theme.dim, &mut blocks);
                }
                push(&cell.background, theme.dim, &mut blocks);
            }
            None => {
                if let Some(output) = &tc.output {
                    let style = if tc.status == ToolCallStatus::Error {
                        theme.dim
                    } else {
                        theme.tool_output
                    };
                    push(output, style, &mut blocks);
                }
            }
        }
        Self {
            blocks,
            line_count: raw_lines,
        }
    }
}

fn is_edit_confirmation(result: &str, cell: &mush_tools::ipython::CellDetails) -> bool {
    let stripped = result.trim().trim_matches(|c| c == '\'' || c == '"');
    cell.diffs
        .iter()
        .any(|diff| stripped == format!("edited {}", diff.path))
}

fn is_send_receipt(result: &str, cell: &mush_tools::ipython::CellDetails) -> bool {
    !cell.sent_messages.is_empty() && result.trim_start().starts_with("{'receipts'")
}

fn render_sent_messages(
    cell: &mush_tools::ipython::CellDetails,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    style: &MessageStyle<'_>,
    gutter: &mut OutputGutter,
) {
    let theme = style.theme;
    for sent in &cell.sent_messages {
        let gutter_text = gutter.next(lines);
        push_gutter(
            lines,
            gutter_text,
            theme.dim,
            vec![
                Span::styled("agent message sent", theme.tool_output),
                Span::styled(format!(" {HEADER_SEPARATOR} {}", sent.receiver), theme.dim),
                Span::styled(format!(" ({})", sent.status), theme.dim),
            ],
            width,
        );
        if style.detail.shows_full_output() {
            for text_line in sent.text.lines() {
                let gutter_text = gutter.next(lines);
                push_gutter(
                    lines,
                    gutter_text,
                    theme.dim,
                    vec![Span::styled(text_line.to_string(), theme.dim)],
                    width,
                );
            }
        }
    }
}

fn render_diffs(
    cell: &mush_tools::ipython::CellDetails,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    style: &MessageStyle<'_>,
    gutter: &mut OutputGutter,
) {
    let theme = style.theme;
    let mut by_path: Vec<(&str, Vec<&str>)> = Vec::new();
    for diff in &cell.diffs {
        match by_path.iter_mut().find(|(path, _)| *path == diff.path) {
            Some((_, diffs)) => diffs.push(&diff.diff),
            None => by_path.push((&diff.path, vec![&diff.diff])),
        }
    }
    for (path, diffs) in by_path {
        let (added, removed) = diffs.iter().fold((0, 0), |(a, r), diff| {
            let (da, dr) = count_changed_lines(diff);
            (a + da, r + dr)
        });
        let gutter_text = gutter.next(lines);
        push_gutter(
            lines,
            gutter_text,
            theme.dim,
            vec![
                Span::styled(path.to_string(), theme.tool_output),
                Span::raw(" "),
                Span::styled(format!("+{added}"), theme.diff_added),
                Span::raw(" "),
                Span::styled(format!("-{removed}"), theme.diff_removed),
            ],
            width,
        );
        if !style.detail.shows_full_output() {
            continue;
        }
        let lang = crate::syntax::lang_from_path(path);
        let inner = width.saturating_sub(1 + INDENT.len()).max(1);
        for diff in diffs {
            for row in super::super::diff::render_diff_with_lang_mode(
                diff,
                inner,
                theme,
                lang,
                super::super::diff::DiffLayout::Inline,
            ) {
                let mut spans = vec![Span::raw(" "), Span::raw(INDENT)];
                spans.extend(row.spans);
                lines.push(Line::from(spans));
            }
        }
    }
}

fn count_changed_lines(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+ ") {
            (added + 1, removed)
        } else if line.starts_with("- ") {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

struct OutputGutter {
    marker_pending: bool,
    blank_pending: bool,
}

impl OutputGutter {
    fn new(has_code: bool) -> Self {
        Self {
            marker_pending: true,
            blank_pending: has_code,
        }
    }

    fn next(&mut self, lines: &mut Vec<Line<'static>>) -> &'static str {
        if self.blank_pending {
            self.blank_pending = false;
            lines.push(Line::raw(""));
        }
        if self.marker_pending {
            self.marker_pending = false;
            OUTPUT_GUTTER
        } else {
            INDENT
        }
    }
}

fn push_gutter(
    lines: &mut Vec<Line<'static>>,
    gutter: &'static str,
    gutter_style: Style,
    spans: Vec<Span<'static>>,
    width: usize,
) {
    let available = width.saturating_sub(1 + gutter.chars().count()).max(1);
    for (index, row) in wrap_styled_spans(spans, available, Style::default())
        .into_iter()
        .enumerate()
    {
        let prefix = if index == 0 {
            Span::styled(gutter, gutter_style)
        } else {
            Span::raw(" ".repeat(gutter.chars().count()))
        };
        let mut out = vec![Span::raw(" "), prefix];
        out.extend(row);
        lines.push(Line::from(out));
    }
}

fn truncate_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    if spans_cell_width(&spans) <= width {
        return Line::from(spans);
    }
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let span_width = spans_cell_width(std::slice::from_ref(&span));
        if used + span_width <= width {
            used += span_width;
            out.push(span);
            continue;
        }
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = char_cell_width(ch);
            if used + w > width {
                break;
            }
            used += w;
            text.push(ch);
        }
        out.push(Span::styled(text, span.style));
        break;
    }
    Line::from(out)
}

pub(super) fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1000 {
        format!("{duration_ms}ms")
    } else {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    }
}
