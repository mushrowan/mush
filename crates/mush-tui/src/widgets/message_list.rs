//! message list widget - renders conversation history

use mush_ai::types::TokenCount;
use ratatui::buffer::{Buffer, CellDiffOption, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Widget;

use crate::app::{
    App, DisplayMessage, DisplayToolCall, ImageRenderArea, MessageRole, ScrollPosition,
    ToolCallStatus,
};
use crate::app_state::CachedHeight;
use crate::widgets::shimmer::shimmer_spans;

mod layout;

pub(crate) use layout::DocumentLayout;
use layout::anchor_row;

#[cfg(test)]
thread_local! {
    static LAST_MATERIALISED_LINE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_materialised_line_count(count: usize) {
    LAST_MATERIALISED_LINE_COUNT.with(|cell| cell.set(count));
}

#[cfg(test)]
fn last_materialised_line_count() -> usize {
    LAST_MATERIALISED_LINE_COUNT.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn record_materialised_line_count(_count: usize) {}

/// horizontal padding cells reserved around message body content.
///
/// the left side is the indent span (`" "`) prepended by `indent_line`.
/// the right side is currently 0 (content may run flush to the right
/// edge). changing this one constant rebalances every wrap/height
/// calculation so the indent math stays declared in exactly one place
pub(crate) const MESSAGE_INDENT_LEFT: usize = 1;
pub(crate) const MESSAGE_INDENT_RIGHT: usize = 1;

/// usable width for message body content given an outer column width.
/// subtracts both the left indent and any right-side reservation so
/// renderers and height estimators agree on wrap points
#[must_use]
pub(crate) fn message_content_width(outer_width: usize) -> usize {
    outer_width.saturating_sub(MESSAGE_INDENT_LEFT + MESSAGE_INDENT_RIGHT)
}

/// minimum leading-space count across non-empty lines.
///
/// Claude's thinking content commonly prefixes every line with a
/// single space. when we then prepend our own indent span, the body
/// visually appears shifted right relative to the header. strip the
/// common prefix so relative indentation inside the thinking block
/// is still preserved
#[must_use]
pub(crate) fn common_leading_space(text: &str) -> usize {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0)
}

/// drop up to `count` leading space characters from `line`. lines with
/// fewer leading spaces than `count` have all their leading whitespace
/// removed; non-space characters are left untouched
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
pub(crate) fn reset_count_estimated_lines_calls() {
    COUNT_ESTIMATED_LINES_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn count_estimated_lines_call_count() -> usize {
    COUNT_ESTIMATED_LINES_CALLS.with(Cell::get)
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

/// renders the full message list including any active stream
pub struct MessageList<'a> {
    app: &'a App,
}

impl<'a> MessageList<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }
}

/// tracks where cached content was replaced with placeholders
/// so we can overlay it directly after the main line pass
struct DeferredCacheRender {
    msg_idx: usize,
    /// absolute document line where the cached lines begin (before padding)
    start_line: usize,
    line_count: usize,
}

#[derive(Debug, Clone, Copy)]
struct MaterialisedLineSegment {
    doc_start: usize,
    line_start: usize,
    line_count: usize,
}

impl MaterialisedLineSegment {
    fn line_for_doc(self, doc_line: usize) -> Option<usize> {
        let offset = doc_line.checked_sub(self.doc_start)?;
        (offset < self.line_count).then_some(self.line_start + offset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectedBlockInfo {
    msg_idx: usize,
    block_idx_in_msg: usize,
}

impl SelectedBlockInfo {
    fn for_app(app: &App) -> Option<Self> {
        if app.interaction.mode != crate::app::AppMode::Scroll
            || app.navigation.scroll_unit != crate::app::ScrollUnit::Block
        {
            return None;
        }

        let selected_block = app.navigation.selected_block?;
        // map the global block index onto its message using the cached
        // block list. blocks belonging to one message are contiguous, so
        // the within-message index is the offset from that message's first
        // block. shares `App::blocks` so navigation and highlight agree on
        // one segmentation instead of rescanning every message each frame
        let blocks = app.blocks();
        let msg_idx = blocks.get(selected_block)?.msg_idx;
        let first = blocks.iter().position(|b| b.msg_idx == msg_idx)?;
        Some(Self {
            msg_idx,
            block_idx_in_msg: selected_block - first,
        })
    }
}

/// what the virtual-scroll pass decided before any line is materialised
struct Viewport {
    /// per-message height index for the whole document
    layout: DocumentLayout,
    /// document row the viewport's top edge should land on, in the
    /// estimate space of `layout`. `None` while following the bottom,
    /// which can only be resolved against the final line count
    target_top: Option<usize>,
    /// document row range worth materialising: the viewport plus a
    /// viewport of margin either side
    render_start: usize,
    render_end: usize,
}

impl Viewport {
    /// estimate per-message heights and work out which document rows
    /// overlap the viewport, so off-screen messages skip markdown
    /// rendering and `indent_line` entirely
    fn compute(app: &App, area: Rect) -> Self {
        let layout = DocumentLayout::build(app, area.width);
        let top = layout.top_row(app.scroll, area.height);
        let vis_h = area.height as usize;
        let margin = vis_h;

        Self {
            target_top: (!app.scroll.is_bottom()).then_some(top),
            render_start: top.saturating_sub(margin),
            render_end: (top + vis_h + margin).min(layout.trailing_start()),
            layout,
        }
    }
}

/// where the frame's content ends up on screen once actual line counts
/// are known
struct FrameGeometry {
    /// blank rows inserted above the content so a short conversation sits
    /// near the input box rather than the top of the pane
    padding: usize,
    /// first document row to paint, i.e. what ratatui would call the
    /// paragraph scroll
    scroll: u16,
    /// height of the message area this frame
    visible: u16,
}

/// resolve the frame's scroll position against the actual materialised
/// line count, publish the geometry other widgets read, and hand back
/// what the paint pass needs.
///
/// `target_top` is the document row the anchor resolved to once actual
/// heights corrected the estimates the layout pass worked from, or `None`
/// while following the bottom
fn settle_geometry(
    app: &App,
    area: Rect,
    target_top: Option<usize>,
    content_doc_len: usize,
    mut message_ranges: Vec<crate::app::MessageRowRange>,
) -> FrameGeometry {
    let content_lines = clamp_usize_to_u16(content_doc_len);
    let visible = area.height;
    let padding = if content_lines < visible {
        (visible - content_lines) as usize
    } else {
        0
    };

    // per-message wrapped-line ranges for mouse hit testing. ranges are
    // absolute document rows, not offsets into the materialised lines, so
    // off-screen messages need no placeholder rows
    if !message_ranges.is_empty() && area.width > 0 {
        if padding > 0 {
            let padding_u16 = clamp_usize_to_u16(padding);
            for range in &mut message_ranges {
                range.start = range.start.saturating_add(padding_u16);
                range.end = range.end.saturating_add(padding_u16);
            }
        }
        *app.render_state.message_row_ranges.borrow_mut() = message_ranges;
    } else {
        app.render_state.message_row_ranges.borrow_mut().clear();
    }

    let total_lines = content_lines + padding as u16;
    let max_scroll = total_lines.saturating_sub(visible);
    let scroll = target_top.map_or(max_scroll, |top| clamp_usize_to_u16(top).min(max_scroll));

    app.render_state.total_content_lines.set(total_lines);
    app.render_state.message_area.set(area);
    app.render_state.render_scroll.set(scroll);
    tracing::trace!(
        target: "mush_tui::scroll",
        position = ?app.scroll,
        target_top,
        render_scroll = scroll,
        max_scroll,
        total_lines,
        visible,
        area_w = area.width,
        area_h = area.height,
        "frame settled"
    );

    FrameGeometry {
        padding,
        scroll,
        visible,
    }
}

/// the selection state both materialisation passes need, resolved once per
/// frame instead of per message
struct SelectionContext {
    in_scroll_mode: bool,
    range: Option<(usize, usize)>,
    selected_block: Option<SelectedBlockInfo>,
    has_visual: bool,
    cursor: Option<usize>,
}

impl SelectionContext {
    fn for_app(app: &App) -> Self {
        Self {
            in_scroll_mode: app.interaction.mode == crate::app::AppMode::Scroll,
            range: app.selection_range(),
            selected_block: SelectedBlockInfo::for_app(app),
            has_visual: app.has_selection(),
            cursor: app.navigation.selected_message,
        }
    }

    /// how message `msg_idx` should be highlighted this frame
    fn hint(&self, msg_idx: usize) -> SelectionHint {
        let in_range = self
            .range
            .is_some_and(|(start, end)| msg_idx >= start && msg_idx <= end);
        let is_cursor = self.cursor == Some(msg_idx);
        SelectionHint {
            selected: self.in_scroll_mode && (is_cursor || in_range),
            is_cursor: self.in_scroll_mode && is_cursor,
            has_visual: self.has_visual,
        }
    }
}

/// line buffers and per-message metadata accumulated while materialising a
/// frame. the body pass fills it, the trailing pass appends, the paint pass
/// reads it
struct FrameLines {
    lines: Vec<Line<'static>>,
    images: Vec<ImagePlaceholder>,
    deferred: Vec<DeferredCacheRender>,
    segments: Vec<MaterialisedLineSegment>,
    ranges: Vec<crate::app::MessageRowRange>,
}

impl FrameLines {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            images: Vec::new(),
            deferred: Vec::new(),
            segments: Vec::new(),
            ranges: Vec::new(),
        }
    }
}

impl MessageList<'_> {
    /// write the materialised lines into the buffer, place any images, and
    /// overlay the selection highlight.
    ///
    /// bypasses ratatui's `Paragraph` on purpose: lines are already wrapped
    /// to the content width by `indent_line`, so Paragraph's LineTruncator
    /// would re-grapheme-scan text that already fits. profiling put that at
    /// ~40% of main-thread CPU in long sessions
    fn paint(
        &self,
        buf: &mut Buffer,
        area: Rect,
        frame: &FrameLines,
        geometry: &FrameGeometry,
        sel_ctx: &SelectionContext,
        img_y_positions: &[usize],
    ) {
        let FrameGeometry {
            padding,
            scroll,
            visible,
        } = *geometry;
        // compute image render areas based on scroll position
        let mut render_areas = Vec::new();
        if !frame.images.is_empty() && area.width > 0 {
            let scroll_i = scroll as i32;
            let padding_i = padding as i32;
            let visible_i = visible as i32;
            for (i, ph) in frame.images.iter().enumerate() {
                let y_before = img_y_positions[i].min(i32::MAX as usize) as i32;
                // placeholder line_idx now points directly at the first body
                // row, so no +1 offset
                let doc_top = padding_i + y_before;
                let doc_bot = doc_top + ph.cell_height as i32;
                let screen_top = doc_top - scroll_i;
                let screen_bot = doc_bot - scroll_i;
                // hide-on-partial: only render when fully inside the
                // viewport. ratatui_image's Fit resize would squish a
                // partial rect, and cropping needs upstream support
                // (see untracked-docs/todo.md)
                if screen_top < 0 || screen_bot > visible_i {
                    continue;
                }
                let visible_y = area.y + screen_top as u16;
                let img_x = area.x + ph.x_offset;
                let img_w = ph.cell_width;
                if img_w > 0 {
                    let area = Rect::new(img_x, visible_y, img_w, ph.cell_height);
                    render_areas.push(ImageRenderArea {
                        msg_idx: ph.msg_idx,
                        tc_idx: ph.tc_idx,
                        area,
                    });
                    mark_image_overlay_cells(area, buf);
                }
            }
        }
        *self.app.render_state.image_render_areas.borrow_mut() = render_areas;

        // render lines directly to the buffer, bypassing ratatui's
        // `Paragraph`. lines are pre-wrapped to `content_width` by
        // `indent_line`, so Paragraph's internal `LineTruncator` would
        // just re-grapheme-scan content that already fits. profiling
        // showed this accounted for ~40% of main-thread CPU in long
        // sessions (LineTruncator::next_line + Graphemes::next +
        // unicode_width::lookup_width). writing via `set_line` skips
        // the whole reflow pipeline
        let scroll_usize = scroll as usize;
        let visible_usize = visible as usize;
        for screen_y in 0..visible_usize {
            let doc_y = scroll_usize + screen_y;
            if doc_y < padding {
                continue; // blank padding row, buffer is already empty
            }
            let doc_line = doc_y - padding;
            let Some(line_idx) = frame
                .segments
                .iter()
                .find_map(|segment| segment.line_for_doc(doc_line))
            else {
                continue;
            };
            buf.set_line(
                area.x,
                area.y + screen_y as u16,
                &frame.lines[line_idx],
                area.width,
            );
        }

        // overlay deferred cached content directly to the buffer.
        // during the lines build, cached content was replaced with empty
        // placeholders to avoid deep-cloning Line vecs. now we borrow
        // from the indented_cache and write directly, zero clones
        if !frame.deferred.is_empty() {
            let scroll_usize = scroll as usize;
            let visible_usize = area.height as usize;
            let cache = self.app.render_state.indented_cache.borrow();
            for dc in &frame.deferred {
                if let Some(Some((_, _, cached_lines))) = cache.get(dc.msg_idx) {
                    let doc_y = padding + dc.start_line;
                    let doc_end = doc_y + dc.line_count;
                    if doc_end > scroll_usize && doc_y < scroll_usize + visible_usize {
                        let skip = scroll_usize.saturating_sub(doc_y);
                        let screen_y = (doc_y.saturating_sub(scroll_usize)) as u16;
                        write_lines_to_buf(&cached_lines[skip..], buf, area, area.y + screen_y);
                    }
                }
            }
        }

        // highlight rows belonging to selected messages / visual range so the
        // user can see exactly what `y` will copy. block-mode highlighting is
        // still handled inline during line building (styled spans) because it
        // needs to colour only the code-block portion of a message.
        if sel_ctx.in_scroll_mode
            && self.app.navigation.scroll_unit != crate::app::ScrollUnit::Block
        {
            apply_message_highlight(
                self.app,
                buf,
                area,
                scroll,
                &self.app.render_state.message_row_ranges.borrow(),
            );
        }
    }

    /// materialise everything below the message list: in-flight assistant
    /// text and thinking, the tool-args preview, the compaction label, and
    /// any queued steering messages
    fn materialise_trailing(&self, area: Rect, sel_ctx: &SelectionContext, frame: &mut FrameLines) {
        frame.lines.extend(streaming_lines(self.app, area.width));

        // queued (steering) messages always appear at the bottom
        for (i, msg) in self.app.messages.iter().enumerate() {
            if !msg.queued {
                continue;
            }
            let sel = sel_ctx.hint(i);
            render_message(
                self.app,
                msg,
                i,
                frame,
                sel,
                area.width,
                sel_ctx.selected_block,
            );
            frame.lines.push(Line::raw(""));
        }
    }
}

impl Widget for MessageList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let sel_ctx = SelectionContext::for_app(self.app);
        let viewport = Viewport::compute(self.app, area);
        let mut frame = FrameLines::new();

        // body: every stored message overlapping the render window
        let layout = &viewport.layout;
        let (render_start, render_end) = (viewport.render_start, viewport.render_end);
        // the anchored message's document row shifts when messages above it
        // inside the window render taller or shorter than the layout pass
        // estimated. track the corrected row so the paint pass puts the
        // anchor exactly on the viewport's top edge
        let mut corrected_top = None;
        let mut visible_height_delta = 0isize;
        for entry in &layout.entries()[layout.visible_entry_range(render_start, render_end)] {
            let Some(msg) = self.app.messages.get(entry.msg_idx) else {
                continue;
            };
            if msg.queued {
                continue; // rendered after streaming content
            }
            let msg_start = offset_usize(entry.start, visible_height_delta);
            let estimated_height = entry.height;
            let start_line = frame.lines.len();

            let image_start = frame.images.len();
            let deferred_start = frame.deferred.len();
            let sel = sel_ctx.hint(entry.msg_idx);
            render_message(
                self.app,
                msg,
                entry.msg_idx,
                &mut frame,
                sel,
                area.width,
                sel_ctx.selected_block,
            );
            frame.lines.push(Line::raw(""));

            let actual_height = frame.lines.len() - start_line;
            if let ScrollPosition::Message { msg: anchor, line } = self.app.scroll
                && anchor == entry.msg_idx
            {
                corrected_top = Some(anchor_row(msg_start, actual_height, line));
            }
            absolutise_render_metadata(
                msg_start,
                start_line,
                &mut frame.images[image_start..],
                &mut frame.deferred[deferred_start..],
            );
            frame.segments.push(MaterialisedLineSegment {
                doc_start: msg_start,
                line_start: start_line,
                line_count: actual_height,
            });
            // the cache may hold an estimate from count_estimated_lines
            // (which counts raw markdown chars), so feed the actual line
            // count back and let it converge after one render
            if actual_height != estimated_height {
                update_cached_height(
                    msg,
                    entry.msg_idx,
                    area.width,
                    actual_height,
                    &self.app.render_state,
                );
            }
            visible_height_delta += actual_height as isize - estimated_height as isize;
            frame.ranges.push(crate::app::MessageRowRange {
                msg_idx: entry.msg_idx,
                start: clamp_usize_to_u16(msg_start),
                end: clamp_usize_to_u16(msg_start + actual_height),
            });
        }

        // trailing: streaming content and queued messages, positioned after
        // the last stored message
        let trailing_doc_start = offset_usize(layout.trailing_start(), visible_height_delta);
        let trailing_line_start = frame.lines.len();
        let trailing_image_start = frame.images.len();
        let trailing_deferred_start = frame.deferred.len();
        self.materialise_trailing(area, &sel_ctx, &mut frame);
        let trailing_line_count = frame.lines.len() - trailing_line_start;
        if trailing_line_count > 0 {
            absolutise_render_metadata(
                trailing_doc_start,
                trailing_line_start,
                &mut frame.images[trailing_image_start..],
                &mut frame.deferred[trailing_deferred_start..],
            );
            frame.segments.push(MaterialisedLineSegment {
                doc_start: trailing_doc_start,
                line_start: trailing_line_start,
                line_count: trailing_line_count,
            });
        }
        if let ScrollPosition::Trailing { line } = self.app.scroll {
            corrected_top = Some(trailing_doc_start + line as usize);
        }
        record_materialised_line_count(frame.lines.len());

        // pre-compute y positions for image placeholders before moving lines
        // into Text. since indent_line and wrap_text pre-wrap all lines to fit
        // within area.width, each line occupies exactly one row
        let w = area.width as usize;
        let img_y_positions: Vec<usize> = if !frame.images.is_empty() && w > 0 {
            frame.images.iter().map(|ph| ph.line_idx).collect()
        } else {
            Vec::new()
        };

        let geometry = settle_geometry(
            self.app,
            area,
            corrected_top.or(viewport.target_top),
            trailing_doc_start + trailing_line_count,
            std::mem::take(&mut frame.ranges),
        );
        self.paint(buf, area, &frame, &geometry, &sel_ctx, &img_y_positions);
    }
}

fn clamp_usize_to_u16(value: usize) -> u16 {
    value.min(u16::MAX as usize) as u16
}

fn offset_usize(value: usize, delta: isize) -> usize {
    if delta >= 0 {
        value.saturating_add(delta as usize)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

fn absolutise_render_metadata(
    doc_start: usize,
    line_start: usize,
    image_placeholders: &mut [ImagePlaceholder],
    deferred_renders: &mut [DeferredCacheRender],
) {
    for placeholder in image_placeholders {
        placeholder.line_idx = doc_start + placeholder.line_idx.saturating_sub(line_start);
    }
    for deferred in deferred_renders {
        deferred.start_line = doc_start + deferred.start_line.saturating_sub(line_start);
    }
}

fn mark_image_overlay_cells(area: Rect, buf: &mut Buffer) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

/// height reserved for inline image rendering (in lines)
const IMAGE_HEIGHT: u16 = 12;

/// height of an image body inside a user-message attachment cell (excludes
/// the top border; the shared divider row with the message box is free)
const ATTACH_INNER_H: u16 = 8;

/// tracks where an image placeholder starts in the lines vec
struct ImagePlaceholder {
    msg_idx: usize,
    tc_idx: usize,
    /// line index in the lines vec where the image body starts (not the
    /// label or top border)
    line_idx: usize,
    /// column offset (relative to the render area x) where the image cell
    /// begins. used for per-attachment cells in user messages so each
    /// image lands inside its own cell instead of across the full width
    x_offset: u16,
    /// cell width in columns (image render width)
    cell_width: u16,
    /// image body height in rows
    cell_height: u16,
}

/// scroll/selection state passed to render_message
struct SelectionHint {
    /// message is highlighted (cursor or within visual range)
    selected: bool,
    /// message is the cursor position (shows hint text)
    is_cursor: bool,
    /// visual selection is active
    has_visual: bool,
}

/// the in-flight reply, the tool-args preview, and the compaction label.
/// redrawn every frame, so none of it is ever committed to scrollback
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

    // streaming tool args (model is building tool call, not yet executing)
    if app.active_tools.is_empty() && !app.stream.tool_args.is_empty() {
        // show a truncated preview of the args being built
        let preview = truncate_line(&app.stream.tool_args, 60);
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(" building ", app.theme.dim),
            Span::styled(preview, app.theme.dim),
        ]));
    }

    // backgrounded /compact run: a single busy-label row
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

/// everything below committed history: the live turn's messages, the
/// streaming block, and any queued steering the user has typed ahead
pub fn live_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let style = MessageStyle {
        theme: &app.theme,
        show_usage: app.interaction.show_usage_lines,
    };
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

/// everything a message needs to render itself, with no viewport state
pub struct MessageStyle<'a> {
    pub theme: &'a Theme,
    pub show_usage: bool,
}

/// render one message to owned lines at `width`.
///
/// takes no `App`: selection, scroll position, and the line caches live
/// in the caller, so these lines can go straight into the terminal's
/// scrollback. image cells are reported but not drawn
pub fn message_lines(
    msg: &DisplayMessage,
    style: &MessageStyle<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // image cells are positions for a live overlay to draw over, keyed by
    // message index. scrollback can't host one, so both are discarded
    let mut images = Vec::new();

    if matches!(msg.role, MessageRole::System) {
        lines.push(system_label_line(style.theme, None));
    }
    push_thinking_lines(msg, style.theme, width, &mut lines);

    if msg.queued {
        push_queued_lines(msg, style.theme, width, &mut lines);
    } else if matches!(msg.role, MessageRole::User) {
        render_user_message_box(style.theme, msg, 0, width, &mut lines, &mut images);
    } else {
        let content_width = message_content_width(width as usize);
        let md = crate::markdown::render_with_width(&msg.content, style.theme, Some(content_width));
        for line in md.lines {
            lines.extend(indent_line(line, content_width));
        }
    }

    push_completed_tool_box_lines(style.theme, msg, width, &mut lines);
    push_usage_line(msg, style, &mut lines);
    lines
}

/// the `system` label, optionally behind a selection marker
fn system_label_line(theme: &Theme, marker: Option<Span<'static>>) -> Line<'static> {
    let mut spans = Vec::new();
    spans.extend(marker);
    spans.push(Span::styled("system", theme.system_label));
    Line::from(spans)
}

/// what sits between a label and what it qualifies in a header
const HEADER_SEPARATOR: &str = " \u{b7} ";

/// one line standing in for a collapsed thinking trace: the last bold
/// section header, else the first line, stripped of markdown
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

/// a line that is one run of bold text, optionally ending with a colon
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
    theme: &Theme,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let Some(ref thinking) = msg.thinking else {
        return;
    };
    let cw = message_content_width(width as usize);
    if !msg.thinking_expanded {
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

/// token and cost summary. held back while a tool is still running so it
/// doesn't appear above the running tool panel
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

/// the marker above a selected message, with the keys that act on it
fn selection_hint_line(app: &App, sel: SelectionHint) -> Line<'static> {
    let mut spans = vec![Span::styled("▌", app.theme.selection_marker)];
    if sel.is_cursor {
        let hint = if app.navigation.scroll_unit == crate::app::ScrollUnit::Block {
            " (y to copy block, b for messages)"
        } else if sel.has_visual {
            " (y to copy range)"
        } else {
            " (v to select, y to copy)"
        };
        spans.push(Span::styled(hint, app.theme.scroll_hint));
    }
    Line::from(spans)
}

fn render_message(
    app: &App,
    msg: &DisplayMessage,
    msg_idx: usize,
    frame: &mut FrameLines,
    sel: SelectionHint,
    width: u16,
    selected_block_info: Option<SelectedBlockInfo>,
) {
    let FrameLines {
        lines,
        images: image_placeholders,
        deferred,
        ..
    } = frame;
    // user and assistant messages have no label line.
    // user messages are distinguished by a subtle background.
    // only system messages get a text label.
    let is_user = matches!(msg.role, MessageRole::User);

    if matches!(msg.role, MessageRole::System) {
        let marker = sel
            .selected
            .then(|| Span::styled("▌ ", app.theme.selection_marker));
        lines.push(system_label_line(&app.theme, marker));
    } else if sel.selected {
        lines.push(selection_hint_line(app, sel));
    }

    push_thinking_lines(msg, &app.theme, width, lines);

    // main content (markdown rendered)
    if msg.queued {
        push_queued_lines(msg, &app.theme, width, lines);
    } else if is_user {
        render_user_message_box(&app.theme, msg, msg_idx, width, lines, image_placeholders);
    } else {
        // determine if a code block in this message is selected
        let highlight_block = selected_block_info
            .filter(|selected| selected.msg_idx == msg_idx)
            .map(|selected| selected.block_idx_in_msg);

        let content_width = message_content_width(width as usize);
        let highlight_range =
            highlight_block.and_then(|b| block_line_ranges(&msg.content).get(b).copied());

        if highlight_range.is_none() {
            // fast path: serve cached indented lines for stable messages
            let hash = cached_content_hash(&msg.content, msg_idx, &app.render_state);
            let width_u16 = content_width as u16;
            let cache_hit_count = {
                let cache = app.render_state.indented_cache.borrow();
                cache
                    .get(msg_idx)
                    .and_then(|e| e.as_ref())
                    .filter(|(h, w, _)| *h == hash && *w == width_u16)
                    .map(|(_, _, cached_lines)| cached_lines.len())
            };
            if let Some(line_count) = cache_hit_count {
                // push empty placeholders instead of deep-cloning.
                // the overlay pass after Paragraph will write cached
                // content directly to the buffer
                let start_line = lines.len();
                lines.resize_with(lines.len() + line_count, || Line::raw(""));
                deferred.push(DeferredCacheRender {
                    msg_idx,
                    start_line,
                    line_count,
                });
            } else {
                let md_text = render_markdown_cached(app, &msg.content, content_width);
                let mut indented = Vec::new();
                for line in md_text.lines {
                    indented.extend(indent_line(line, content_width));
                }
                lines.extend(indented.iter().cloned());
                let mut cache = app.render_state.indented_cache.borrow_mut();
                if cache.len() <= msg_idx {
                    cache.resize_with(msg_idx + 1, || None);
                }
                cache[msg_idx] = Some((hash, width_u16, indented));
            }
        } else {
            // block highlight active: compute fresh with style overrides
            let md_text = render_markdown_cached(app, &msg.content, content_width);
            for (i, line) in md_text.lines.into_iter().enumerate() {
                let style_override = if let Some((start, end)) = highlight_range
                    && i >= start
                    && i < end
                {
                    Some(Style::default().bg(app.theme.block_highlight_bg))
                } else {
                    None
                };

                for mut indented in indent_line(line, content_width) {
                    if let Some(style) = style_override {
                        indented = indented.style(style);
                    }
                    lines.push(indented);
                }
            }
        }
    }

    render_completed_tool_boxes_cached(app, msg, msg_idx, width, lines, image_placeholders);

    push_usage_line(
        msg,
        &MessageStyle {
            theme: &app.theme,
            show_usage: app.interaction.show_usage_lines,
        },
        lines,
    );
}

/// render a user message as a rounded-cornered box with optional
/// attachment row above. attachments share the message box's top border
/// via left/right tees so the visual is a unified stack
/// two columns of padding either side of the text
const USER_PAD: usize = 2;

/// the user's message as a tinted block.
///
/// attached images get a row of placeholder cells above the text for the
/// overlay to draw into
fn render_user_message_box(
    theme: &Theme,
    msg: &DisplayMessage,
    msg_idx: usize,
    width: u16,
    lines: &mut Vec<Line<'static>>,
    image_placeholders: &mut Vec<ImagePlaceholder>,
) {
    let bg = theme.user_msg_bg;
    let w = width as usize;
    let content_width = w.saturating_sub(USER_PAD * 2).max(1);
    let blank = || Line::styled(" ".repeat(w), bg);

    if !msg.images.is_empty() {
        let cell_widths = compute_attach_cell_widths(content_width, msg.images.len());
        let body_start = lines.len();
        for _ in 0..ATTACH_INNER_H {
            lines.push(blank());
        }
        let mut x_offset = USER_PAD as u16;
        for (tc_idx, cw) in cell_widths.iter().enumerate() {
            image_placeholders.push(ImagePlaceholder {
                msg_idx,
                tc_idx,
                line_idx: body_start,
                x_offset,
                cell_width: *cw as u16,
                cell_height: ATTACH_INNER_H,
            });
            x_offset = x_offset.saturating_add(*cw as u16);
        }
    }

    lines.push(blank());
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

/// split `total` columns across `n` image cells, widest first
fn compute_attach_cell_widths(total: usize, n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let base = total / n;
    let remainder = total % n;
    (0..n)
        .map(|i| if i < remainder { base + 1 } else { base })
        .collect()
}

/// hash string content for cache keying
fn content_hash(s: &str) -> u64 {
    #[cfg(test)]
    CONTENT_HASH_CALLS.with(|c| c.set(c.get() + 1));

    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// cached content hash: returns the hash from cache if byte length matches,
/// otherwise recomputes and stores. message content is write-once so byte
/// length is a sufficient invalidation key
fn cached_content_hash(
    content: &str,
    msg_idx: usize,
    render_state: &crate::app_state::RenderState,
) -> u64 {
    let len = content.len();
    {
        let cache = render_state.content_hash_cache.borrow();
        if let Some(Some((cached_len, cached_hash))) = cache.get(msg_idx)
            && *cached_len == len
        {
            return *cached_hash;
        }
    }

    let hash = content_hash(content);
    {
        let mut cache = render_state.content_hash_cache.borrow_mut();
        if cache.len() <= msg_idx {
            cache.resize_with(msg_idx + 1, || None);
        }
        cache[msg_idx] = Some((len, hash));
    }
    hash
}

/// build a cheap O(1) fingerprint for height cache lookup.
/// all fields except `height` form the cache key
fn height_fingerprint(msg: &DisplayMessage, width: u16, height: usize) -> CachedHeight {
    let tool_output_len: usize = msg
        .tool_calls
        .iter()
        .filter(|tc| tc.status != ToolCallStatus::Running)
        .map(|tc| tc.output_preview.as_ref().map_or(0, String::len))
        .sum();
    let completed_tool_count = msg
        .tool_calls
        .iter()
        .filter(|tc| tc.status != ToolCallStatus::Running)
        .count();
    let has_usage = msg
        .usage
        .as_ref()
        .is_some_and(|u| u.total_tokens() > TokenCount::ZERO);

    CachedHeight {
        content_len: msg.content.len(),
        thinking_len: msg.thinking.as_ref().map_or(0, String::len),
        tool_output_len,
        completed_tool_count,
        thinking_expanded: msg.thinking_expanded,
        has_usage,
        width,
        height,
    }
}

/// cheap height estimate for viewport culling.
/// uses a per-message cache keyed by O(1) byte-length fingerprint
/// to avoid re-measuring content every frame. falls back to the indented
/// cache or terminal-cell-width estimation on miss.
/// result includes the trailing separator line.
fn estimate_message_height(
    msg: &DisplayMessage,
    msg_idx: usize,
    width: u16,
    render_state: &crate::app_state::RenderState,
) -> usize {
    // check cache
    {
        let cache = render_state.height_cache.borrow();
        if let Some(Some(entry)) = cache.get(msg_idx) {
            let probe = height_fingerprint(msg, width, entry.height);
            if *entry == probe {
                return entry.height;
            }
        }
    }

    let h = compute_message_height(msg, msg_idx, width, render_state);

    // store in cache
    {
        let mut cache = render_state.height_cache.borrow_mut();
        if cache.len() <= msg_idx {
            cache.resize_with(msg_idx + 1, || None);
        }
        cache[msg_idx] = Some(height_fingerprint(msg, width, h));
    }

    h
}

/// update the height_cache entry with the actual rendered line count.
/// called after render_message so the cache converges to exact values
/// even when the initial estimate from count_estimated_lines was wrong
fn update_cached_height(
    msg: &DisplayMessage,
    msg_idx: usize,
    width: u16,
    actual_height: usize,
    render_state: &crate::app_state::RenderState,
) {
    let mut cache = render_state.height_cache.borrow_mut();
    if cache.len() <= msg_idx {
        cache.resize_with(msg_idx + 1, || None);
    }
    cache[msg_idx] = Some(height_fingerprint(msg, width, actual_height));
}

/// inner height computation (called on cache miss)
fn compute_message_height(
    msg: &DisplayMessage,
    msg_idx: usize,
    width: u16,
    render_state: &crate::app_state::RenderState,
) -> usize {
    let cw = message_content_width(width as usize).max(1);
    let mut h = 1; // separator line

    // system label
    if matches!(msg.role, MessageRole::System) {
        h += 1;
    }

    // thinking
    if let Some(ref thinking) = msg.thinking
        && msg.thinking_expanded
    {
        h += count_estimated_lines(thinking, cw);
    }

    // content
    if matches!(msg.role, MessageRole::User) {
        // user msg body width is inner-less-2 (1 space each side)
        let body_width = (width as usize).saturating_sub(4).max(1);
        // rounded top + content + rounded bottom
        h += 2;
        h += count_estimated_lines(&msg.content, body_width);
        // attach row above when images present: top border + inner rows
        // + shared divider. divider counts once (replaces plain top)
        if !msg.images.is_empty() {
            h += 1 /* top border */ + ATTACH_INNER_H as usize + 1 /* divider */;
            // the plain rounded top we counted above is replaced by the
            // divider row, so subtract 1 to avoid double-counting
            h = h.saturating_sub(1);
        }
    } else if !msg.content.is_empty() && !matches!(msg.role, MessageRole::System) {
        // try indented cache for exact content line count
        let exact = {
            let cache = render_state.indented_cache.borrow();
            cache
                .get(msg_idx)
                .and_then(|e| e.as_ref())
                .filter(|(hash, w, _)| {
                    *hash == cached_content_hash(&msg.content, msg_idx, render_state)
                        && *w == cw as u16
                })
                .map(|(_, _, cached_lines)| cached_lines.len())
        };
        h += exact.unwrap_or_else(|| count_estimated_lines(&msg.content, cw));
    }

    // completed tool calls
    for tc in &msg.tool_calls {
        if tc.status != ToolCallStatus::Running {
            h += 3; // top border + summary + bottom border
            if let Some(ref output) = tc.output_preview {
                let inner = (width as usize).saturating_sub(5).max(1);
                h += count_estimated_lines(output, inner);
            }
            if tc.image_data.is_some() {
                h += IMAGE_HEIGHT as usize;
            }
        }
    }

    // usage
    if msg
        .usage
        .as_ref()
        .is_some_and(|u| u.total_tokens() > TokenCount::ZERO)
    {
        h += 1;
    }

    h
}

/// estimate trailing content height appended after the message list:
/// streaming text / thinking, tool-args preview, queued messages.
/// the viewport calc needs this so messages that end up in the actual
/// visible region aren't falsely marked as off-screen
fn estimate_trailing_height(
    app: &App,
    width: u16,
    render_state: &crate::app_state::RenderState,
) -> usize {
    let stream_content_width = message_content_width(width as usize).max(1);
    let mut h = 0;

    if app.stream.active {
        let has_thinking = !app.stream.thinking.is_empty()
            && app.thinking_display != crate::app::ThinkingDisplay::Hidden;
        let has_text = !app.stream.text.is_empty();

        if has_thinking {
            // busy label + indented thinking lines + blank separator
            h += 1;
            h += count_estimated_lines(app.visible_streaming_thinking(), stream_content_width);
            h += 1;
        }
        if has_text {
            h += count_estimated_lines(app.visible_streaming_text(), stream_content_width);
        }
        if !has_thinking && !has_text {
            // "working" busy label
            h += 1;
        }
    }

    // tool args preview line
    if app.active_tools.is_empty() && !app.stream.tool_args.is_empty() {
        h += 1;
    }

    // queued messages at the bottom (rendered via the normal path)
    for (i, msg) in app.messages.iter().enumerate() {
        if msg.queued {
            h += compute_message_height(msg, i, width, render_state);
            h += 1; // trailing blank separator between queued
        }
    }

    h
}

/// estimate how many wrapped lines text occupies at a given width
fn count_estimated_lines(text: &str, width: usize) -> usize {
    #[cfg(test)]
    COUNT_ESTIMATED_LINES_CALLS.with(|c| c.set(c.get() + 1));

    if text.is_empty() || width == 0 {
        return 1;
    }
    text.lines()
        .map(|line| {
            let cells = text_cell_width(line);
            if cells == 0 { 1 } else { cells.div_ceil(width) }
        })
        .sum::<usize>()
        .max(1)
}

/// truncate a string to at most `max` characters (not bytes), adding ellipsis
fn truncate_line(s: &str, max: usize) -> String {
    // take first line, strip whitespace
    let line = s.lines().next().unwrap_or(s).trim();
    let char_count = line.chars().count();
    if char_count <= max {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// render markdown text to styled ratatui Text
fn render_markdown_cached(app: &App, source: &str, width: usize) -> Text<'static> {
    if source.is_empty() {
        return Text::default();
    }

    if let Some(cached) = app
        .render_state
        .markdown_cache
        .borrow()
        .get(&format!("{width}\0{source}"))
        .cloned()
    {
        return cached;
    }

    let rendered = crate::markdown::render_with_width(source, &app.theme, Some(width));
    app.render_state
        .markdown_cache
        .borrow_mut()
        .insert(format!("{width}\0{source}"), rendered.clone());
    rendered
}

fn render_streaming_markdown_cached(app: &App, source: &str, width: usize) -> Text<'static> {
    if source.is_empty() {
        return Text::default();
    }

    // cache against the FULL stream buffer, not the visible (typewriter-truncated)
    // portion. this way typewriter ticks don't invalidate the cache: we only
    // re-parse when new deltas arrive
    let full = &app.stream.text;

    if let Some((cached_width, cached_source, cached)) =
        app.render_state.stream_markdown_cache.borrow().as_ref()
        && *cached_width == width
        && cached_source == full
    {
        // cache hit: truncate the pre-rendered output to the visible char count
        return truncate_text(cached.clone(), source.chars().count());
    }

    let rendered = crate::markdown::render_streaming_with_width(full, &app.theme, Some(width));
    *app.render_state.stream_markdown_cache.borrow_mut() =
        Some((width, full.to_string(), rendered.clone()));
    truncate_text(rendered, source.chars().count())
}

/// truncate a rendered Text to at most `max_chars` visible characters
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
            // truncate this line's spans to fit
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

fn render_completed_tool_boxes_cached(
    app: &App,
    msg: &DisplayMessage,
    msg_idx: usize,
    width: u16,
    lines: &mut Vec<Line<'static>>,
    image_placeholders: &mut Vec<ImagePlaceholder>,
) {
    let Some(hash) = tool_boxes_hash(msg) else {
        return;
    };
    if completed_tool_boxes_have_images(msg) {
        render_completed_tool_boxes(app, msg, msg_idx, width, lines, image_placeholders);
        return;
    }

    if let Some(cached_lines) = app
        .render_state
        .tool_box_cache
        .borrow()
        .get(msg_idx)
        .and_then(|entry| entry.as_ref())
        .filter(|(h, w, _)| *h == hash && *w == width)
        .map(|(_, _, cached)| cached.clone())
    {
        lines.extend(cached_lines);
        return;
    }

    let mut rendered = Vec::new();
    push_completed_tool_box_lines(&app.theme, msg, width, &mut rendered);
    lines.extend(rendered.iter().cloned());

    let mut cache = app.render_state.tool_box_cache.borrow_mut();
    if cache.len() <= msg_idx {
        cache.resize_with(msg_idx + 1, || None);
    }
    cache[msg_idx] = Some((hash, width, rendered));
}

fn render_completed_tool_boxes(
    app: &App,
    msg: &DisplayMessage,
    msg_idx: usize,
    width: u16,
    lines: &mut Vec<Line<'static>>,
    image_placeholders: &mut Vec<ImagePlaceholder>,
) {
    let completed: Vec<(usize, &DisplayToolCall)> = msg
        .tool_calls
        .iter()
        .enumerate()
        .filter(|(_, tc)| tc.status != ToolCallStatus::Running)
        .collect();

    for group in completed.chunk_by(|a, b| a.1.batch == b.1.batch) {
        render_tool_box_group(
            &group.iter().map(|(_, tc)| *tc).collect::<Vec<_>>(),
            width,
            lines,
            &app.theme,
        );
        register_tool_image_placeholders(group, msg_idx, width, lines, image_placeholders, app);
    }
}

fn push_completed_tool_box_lines(
    theme: &Theme,
    msg: &DisplayMessage,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let completed: Vec<&DisplayToolCall> = msg
        .tool_calls
        .iter()
        .filter(|tc| tc.status != ToolCallStatus::Running)
        .collect();

    for group in completed.chunk_by(|a, b| a.batch == b.batch) {
        render_tool_box_group(group, width, lines, theme);
    }
}

fn register_tool_image_placeholders(
    group: &[(usize, &DisplayToolCall)],
    msg_idx: usize,
    width: u16,
    lines: &mut Vec<Line<'_>>,
    image_placeholders: &mut Vec<ImagePlaceholder>,
    app: &App,
) {
    for &(tc_idx, tc) in group {
        if tc.image_data.is_none() {
            continue;
        }

        lines.push(Line::from(vec![
            Span::styled("    📷 ", Style::default()),
            Span::styled("image", app.theme.image_label),
        ]));
        image_placeholders.push(ImagePlaceholder {
            msg_idx,
            tc_idx,
            line_idx: lines.len(),
            x_offset: 4,
            cell_width: width.saturating_sub(8),
            cell_height: IMAGE_HEIGHT.saturating_sub(1),
        });
        for _ in 1..IMAGE_HEIGHT {
            lines.push(Line::raw(""));
        }
    }
}

fn completed_tool_boxes_have_images(msg: &DisplayMessage) -> bool {
    msg.tool_calls
        .iter()
        .any(|tc| tc.status != ToolCallStatus::Running && tc.image_data.is_some())
}

fn tool_boxes_hash(msg: &DisplayMessage) -> Option<u64> {
    let mut has_completed = false;
    let mut h = std::hash::DefaultHasher::new();
    use std::hash::{Hash, Hasher};

    for tc in msg
        .tool_calls
        .iter()
        .filter(|tc| tc.status != ToolCallStatus::Running)
    {
        has_completed = true;
        tc.name.hash(&mut h);
        tc.summary.hash(&mut h);
        tool_status_tag(tc.status).hash(&mut h);
        tc.output_preview.hash(&mut h);
        tc.image_data.as_ref().map(Vec::len).hash(&mut h);
        tc.batch.hash(&mut h);
    }

    has_completed.then(|| h.finish())
}

fn tool_status_tag(status: ToolCallStatus) -> u8 {
    match status {
        ToolCallStatus::Running => 0,
        ToolCallStatus::Done => 1,
        ToolCallStatus::Error => 2,
    }
}

/// render a group of completed tool calls (same batch) as bordered boxes
fn render_tool_box_group(
    tools: &[&DisplayToolCall],
    total_width: u16,
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
) {
    if total_width < 8 {
        return;
    }
    for tc in tools {
        render_tool_panel(tc, total_width as usize, lines, theme);
    }
}

/// two columns of padding either side of a panel's content
const PANEL_PAD: usize = 2;

/// width available to a panel's content
fn panel_content_width(width: usize) -> usize {
    width.saturating_sub(PANEL_PAD * 2).max(1)
}

/// one row of a tool panel: `content` indented and padded out to `width`,
/// all of it on the panel background
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

/// one completed tool: a `name . status` header over its summary and output
fn render_tool_panel(
    tc: &DisplayToolCall,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
) {
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

    if let Some(lang) = tool_summary_language(tc) {
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

    let Some(ref output) = tc.output_preview else {
        return;
    };
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
        "write" | "applypatch" => crate::syntax::lang_from_path(&tc.summary),
        "grep" | "find" | "glob" | "ls" | "repomap" | "websearch" | "webfetch" => Some("text"),
        _ => None,
    }
}

/// push a styled content line inside a bordered box, wrapping if needed
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

/// parse a read-tool output line of the form `L<n>: <content>`. returns
/// the line number and content on success, `None` when the line doesn't
/// start with `L<digits>: `
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

/// widest line number across all `L<n>:` lines in a read-tool output
/// preview. used for right-aligning the dim line-number gutter so
/// numbers stack cleanly regardless of their digit count
#[must_use]
pub(crate) fn read_line_number_width(text: &str) -> usize {
    text.lines()
        .filter_map(parse_read_line)
        .map(|(n, _)| n.to_string().len())
        .max()
        .unwrap_or(0)
}

/// minimum leading-space count across the `L<n>:` content in a read-tool
/// output preview. blank content rows and non-matching rows are skipped
/// so they don't collapse the result to zero. returns 0 when no matching
/// non-blank rows are found
#[must_use]
pub(crate) fn read_min_content_indent(text: &str) -> usize {
    text.lines()
        .filter_map(parse_read_line)
        .filter(|(_, content)| !content.trim().is_empty())
        .map(|(_, content)| content.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0)
}

/// shared rendering state for a read-tool output block. groups the
/// invariant-across-lines arguments that `push_read_content_line`
/// otherwise takes individually (clippy::too_many_arguments).
/// `indent` is stored by value (Span owns its string) so the ctx
/// doesn't inherit the borrow lifetime from the outer call site
struct ReadLineCtx<'a> {
    bg: Style,
    gutter_width: usize,
    /// leading spaces common to every non-blank content row, stripped
    /// before display so the smallest-indent row sits against the gutter
    strip_indent: usize,
    lang: Option<&'a str>,
    theme: &'a Theme,
}

/// push a read-tool output line inside a bordered box: the `L<n>:`
/// prefix becomes a right-aligned dim gutter, the content is
/// syntax-highlighted when `ctx.lang` is known. falls back to plain
/// rendering for lines that don't match the `L<n>:` pattern
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

    // strip the shared leading indent so deeply nested code doesn't sit
    // half-way across the preview box. only applied to non-blank rows so
    // blank-content lines stay blank (their indent was already 0 or less
    // than strip_indent by definition)
    let content = if content.len() >= ctx.strip_indent
        && content.as_bytes()[..ctx.strip_indent]
            .iter()
            .all(|b| *b == b' ')
    {
        &content[ctx.strip_indent..]
    } else {
        content
    };

    // gutter: right-aligned line number followed by 2 spaces of separation
    let gutter = format!("{:>w$}  ", n, w = ctx.gutter_width);
    let gutter_width = text_cell_width(&gutter);
    let remaining_width = inner_width.saturating_sub(gutter_width);

    // build content spans via syntect (bg-free, keeps terminal bg) or plain
    let content_spans: Vec<Span<'static>> = ctx
        .lang
        .and_then(|l| crate::syntax::highlight_line(content, l, ctx.theme))
        .unwrap_or_else(|| vec![Span::raw(content.to_string())]);

    // measure content width so we can pad the tail
    let content_width = spans_cell_width(&content_spans);

    // if content overflows the remaining width, fall back to wrapping
    // without syntax highlighting (simpler and rare for read previews)
    if content_width > remaining_width {
        push_box_content_line(text, inner_width, ctx.bg, ctx.theme.dim, lines);
        return;
    }

    let mut spans = vec![Span::styled(gutter, ctx.theme.dim)];
    spans.extend(content_spans);
    push_panel_line(spans, inner_width + PANEL_PAD * 2, ctx.bg, lines);
}

/// pre-wrap a styled line to `content_width`, prepending a 1-space indent
/// to every resulting line (including continuations from wrapping)
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

    // flatten spans into (char, style, width) triples so word-boundary
    // wrap logic doesn't have to track span boundaries separately
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

    // walk cells, tracking last-space index within the current row so we
    // can backtrack to it when the row overflows. when we do backtrack,
    // the space itself is dropped so the next row doesn't start with a
    // leading ghost space
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style, usize)> = Vec::new();
    let mut row_width: usize = 0;
    let mut last_space: Option<usize> = None;

    for (ch, style, w) in cells {
        if row_width + w > content_width && !row.is_empty() {
            if let Some(sp) = last_space {
                // split at last space: [..sp] becomes a row, sp itself is
                // dropped, [sp+1..] starts the next row
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

    // rebuild Line<'a> for each row, coalescing consecutive same-style chars
    // into Span<'static> to minimise allocations downstream
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

/// drop any trailing spaces from a row so wrap boundaries don't leave
/// ghost whitespace
fn strip_row(row: &[(char, Style, usize)]) -> Vec<(char, Style)> {
    let end = row
        .iter()
        .rposition(|(c, _, _)| *c != ' ')
        .map_or(0, |p| p + 1);
    row[..end].iter().map(|(c, s, _)| (*c, *s)).collect()
}

/// wrap text to fit within `width` terminal cells, breaking at spaces first,
/// then cell-wise for words longer than the width
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
            // push current line first
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            // cell-wrap the long word
            let chunks = split_word_by_cell_width(word, width);
            let last_index = chunks.len().saturating_sub(1);
            for (index, chunk) in chunks.into_iter().enumerate() {
                if index == last_index {
                    // last chunk becomes current line so next word can join
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

/// the word a tool panel header ends with, and the colour it carries
fn tool_status_label(tc: &DisplayToolCall, theme: &Theme) -> (&'static str, Color) {
    match tc.status {
        ToolCallStatus::Done => ("done", theme.tool_done.fg.unwrap_or(Color::Green)),
        ToolCallStatus::Error => ("error", theme.tool_error.fg.unwrap_or(Color::Red)),
        ToolCallStatus::Running => ("running", theme.tool_running.fg.unwrap_or(Color::Cyan)),
    }
}

/// style for a line of tool output (diff-aware)
fn diff_line_style(line: &str, theme: &Theme) -> Style {
    if line.starts_with("+ ") {
        theme.diff_added
    } else if line.starts_with("- ") {
        theme.diff_removed
    } else {
        theme.dim
    }
}

/// compute rendered-line ranges for each selectable block in a
/// message body, in the same order as [`App::blocks`] yields them
/// for that message. paragraphs and fenced bodies both produce
/// ranges. fence marker lines are consumed (not rendered) just as
/// they are by the markdown renderer, so paragraph ranges that
/// border a fence don't include the fence line.
fn block_line_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut in_fence = false;
    let mut rendered_idx = 0usize;
    let mut fence_start = 0usize;
    let mut para_start: Option<usize> = None;

    for line in source.lines() {
        if line.starts_with("```") {
            if in_fence {
                // closing fence: emit fence range
                ranges.push((fence_start + 1, rendered_idx + 1));
                in_fence = false;
            } else {
                // opening fence: flush any open paragraph first
                if let Some(start) = para_start.take() {
                    ranges.push((start, rendered_idx));
                }
                fence_start = rendered_idx;
                in_fence = true;
            }
            // fence markers are consumed, not rendered
            continue;
        }

        if in_fence {
            if rendered_idx == fence_start {
                rendered_idx += 1;
            }
            rendered_idx += 1;
            continue;
        }

        if line.trim().is_empty() {
            // paragraph boundary
            if let Some(start) = para_start.take() {
                ranges.push((start, rendered_idx));
            }
        } else if para_start.is_none() {
            para_start = Some(rendered_idx);
        }
        rendered_idx += 1;
    }

    // tail flushes
    if let Some(start) = para_start {
        ranges.push((start, rendered_idx));
    }
    if in_fence && rendered_idx > fence_start {
        ranges.push((fence_start + 1, rendered_idx + 1));
    }

    ranges
}

/// write borrowed lines directly to a buffer at a y offset.
/// clips to the area height. returns the number of rows written.
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

/// paint a subtle background tint on rows belonging to the selected
/// message (or the visual selection range) so the user sees exactly
/// which content `y` will copy.
///
/// called after the main Paragraph has already drawn text, so we only
/// mutate the `bg` style and preserve foreground content.
fn apply_message_highlight(
    app: &App,
    buf: &mut Buffer,
    area: Rect,
    scroll: u16,
    ranges: &[crate::app::MessageRowRange],
) {
    let (sel_start, sel_end) = match app.selection_range() {
        Some((s, e)) => (s, e),
        None => match app.navigation.selected_message {
            Some(sel) => (sel, sel),
            None => return,
        },
    };

    let bg = app.theme.block_highlight_bg;
    for range in ranges {
        if range.msg_idx < sel_start || range.msg_idx > sel_end {
            continue;
        }
        let doc_start = range.start as u32;
        let doc_end = range.end as u32;
        if doc_end <= scroll as u32 {
            continue;
        }
        let visible_start = doc_start.saturating_sub(scroll as u32);
        let visible_end = doc_end
            .saturating_sub(scroll as u32)
            .min(area.height as u32);
        if visible_start >= visible_end {
            continue;
        }
        for y_offset in visible_start..visible_end {
            let y = area.y + y_offset as u16;
            if y >= area.bottom() {
                break;
            }
            for x in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(bg);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
