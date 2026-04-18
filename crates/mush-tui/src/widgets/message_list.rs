//! message list widget - renders conversation history

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Widget;

use throbber_widgets_tui::{BRAILLE_SIX, Throbber, WhichUse};

use mush_ai::types::TokenCount;

use crate::app::{
    App, CodeBlock, DisplayMessage, DisplayToolCall, ImageRenderArea, MessageRole, ToolCallStatus,
};
use crate::app_state::CachedHeight;

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
    /// position in the flat lines vec (before padding)
    start_line: usize,
    line_count: usize,
}

impl Widget for MessageList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let mut image_placeholders: Vec<ImagePlaceholder> = Vec::new();
        // track where each message starts in the lines vec
        let mut msg_line_starts: Vec<(usize, usize)> = Vec::new();
        let mut deferred_renders: Vec<DeferredCacheRender> = Vec::new();

        let in_scroll_mode = self.app.interaction.mode == crate::app::AppMode::Scroll;
        let selection_range = self.app.selection_range();
        // compute once, not per-message (was O(N²) in block scroll mode)
        let all_blocks = self.app.code_blocks();

        // virtual scrolling: estimate per-message heights and determine
        // which messages overlap the viewport so we can skip expensive
        // markdown rendering and indent_line for off-screen messages
        let heights: Vec<usize> = self
            .app
            .messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                if msg.queued {
                    return 0;
                }
                estimate_message_height(msg, i, area.width, &self.app.render_state)
            })
            .collect();
        let estimated_total: usize = heights.iter().sum();

        // compensate scroll for content growth while the user is scrolled up.
        // scroll_offset is "lines from bottom" so when content grows at the
        // bottom, we need to increase the effective offset to keep the
        // viewport pinned to the same absolute position
        let prev_total = self.app.render_state.prev_content_lines.get();
        let prev_compensation = self.app.render_state.scroll_compensation.get();
        let compensation = if self.app.scroll_offset > 0 && prev_total > 0 {
            prev_compensation + estimated_total.saturating_sub(prev_total)
        } else {
            0
        };
        self.app
            .render_state
            .prev_content_lines
            .set(estimated_total);
        self.app.render_state.scroll_compensation.set(compensation);
        let effective_offset = (self.app.scroll_offset as usize) + compensation;

        let vis_h = area.height as usize;
        let max_scroll = estimated_total.saturating_sub(vis_h);
        let scroll_from_top = max_scroll.saturating_sub(effective_offset);

        // render messages within the viewport plus one viewport of margin
        let margin = vis_h;
        let render_start = scroll_from_top.saturating_sub(margin);
        let render_end = (scroll_from_top + vis_h + margin).min(estimated_total);

        let mut cumulative = 0;
        let mut should_render = vec![false; self.app.messages.len()];
        for (i, &h) in heights.iter().enumerate() {
            let msg_end = cumulative + h;
            if h > 0 && msg_end > render_start && cumulative < render_end {
                should_render[i] = true;
            }
            cumulative = msg_end;
        }

        for (i, msg) in self.app.messages.iter().enumerate() {
            if msg.queued {
                continue; // rendered after streaming content
            }
            let start_line = lines.len();

            if should_render[i] {
                let in_selection =
                    selection_range.is_some_and(|(start, end)| i >= start && i <= end);
                let sel = SelectionHint {
                    selected: in_scroll_mode
                        && (self.app.navigation.selected_message == Some(i) || in_selection),
                    is_cursor: in_scroll_mode && self.app.navigation.selected_message == Some(i),
                    has_visual: self.app.has_selection(),
                };
                render_message(
                    self.app,
                    msg,
                    i,
                    &mut lines,
                    sel,
                    &mut image_placeholders,
                    &mut deferred_renders,
                    area.width,
                    &all_blocks,
                );
                lines.push(Line::raw(""));

                // correct the height_cache with the actual rendered height.
                // the cache may hold a stale estimate from count_estimated_lines
                // (which counts raw markdown chars). updating here ensures the
                // cache converges after one render, preventing persistent gaps
                // in virtual scrolling placeholders
                let actual_height = lines.len() - start_line;
                if actual_height != heights[i] {
                    update_cached_height(msg, i, area.width, actual_height, &self.app.render_state);
                }
            } else {
                // off-screen: use estimated height as placeholder lines
                for _ in 0..heights[i] {
                    lines.push(Line::raw(""));
                }
            }
            msg_line_starts.push((i, start_line));
        }

        // streaming content
        if self.app.stream.active {
            let dim = self.app.theme.dim;
            let throbber = Throbber::default()
                .throbber_set(BRAILLE_SIX)
                .use_type(WhichUse::Spin);
            let spinner_span = throbber.to_symbol_span(&self.app.throbber_state);
            let stream_content_width = (area.width as usize).saturating_sub(1);

            if !self.app.stream.thinking.is_empty()
                && self.app.thinking_display != crate::app::ThinkingDisplay::Hidden
            {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    spinner_span.clone().style(dim),
                    Span::styled(" thinking", dim),
                ]));
                let visible_thinking = self.app.visible_streaming_thinking();
                for text_line in visible_thinking.lines() {
                    let styled = Line::styled(text_line.to_string(), self.app.theme.thinking);
                    lines.extend(indent_line(styled, stream_content_width));
                }
                lines.push(Line::raw(""));
            }
            if !self.app.stream.text.is_empty() {
                let visible_text = self.app.visible_streaming_text();
                let md_text = render_streaming_markdown_cached(self.app, visible_text);
                for line in md_text.lines {
                    lines.extend(indent_line(line, stream_content_width));
                }
            }
            if self.app.stream.text.is_empty() && self.app.stream.thinking.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    spinner_span.clone().style(dim),
                    Span::styled(" working", dim),
                ]));
            }
        }

        // streaming tool args (model is building tool call, not yet executing)
        if self.app.active_tools.is_empty() && !self.app.stream.tool_args.is_empty() {
            let throbber = Throbber::default()
                .throbber_set(BRAILLE_SIX)
                .use_type(WhichUse::Spin);
            let spinner_span = throbber.to_symbol_span(&self.app.throbber_state);
            // show a truncated preview of the args being built
            let preview = truncate_line(&self.app.stream.tool_args, 60);
            lines.push(Line::from(vec![
                Span::raw(" "),
                spinner_span.style(self.app.theme.dim),
                Span::styled(" building ", self.app.theme.dim),
                Span::styled(preview, self.app.theme.dim),
            ]));
        }

        // queued (steering) messages always appear at the bottom
        for (i, msg) in self.app.messages.iter().enumerate() {
            if !msg.queued {
                continue;
            }
            let in_selection = selection_range.is_some_and(|(start, end)| i >= start && i <= end);
            let sel = SelectionHint {
                selected: in_scroll_mode
                    && (self.app.navigation.selected_message == Some(i) || in_selection),
                is_cursor: in_scroll_mode && self.app.navigation.selected_message == Some(i),
                has_visual: self.app.has_selection(),
            };
            render_message(
                self.app,
                msg,
                i,
                &mut lines,
                sel,
                &mut image_placeholders,
                &mut deferred_renders,
                area.width,
                &all_blocks,
            );
            lines.push(Line::raw(""));
        }

        // pre-compute y positions for image placeholders before moving lines
        // into Text. since indent_line and wrap_text pre-wrap all lines to fit
        // within area.width, each line occupies exactly one row
        let w = area.width as usize;
        let img_y_positions: Vec<u16> = if !image_placeholders.is_empty() && w > 0 {
            image_placeholders
                .iter()
                .map(|ph| ph.line_idx as u16)
                .collect()
        } else {
            Vec::new()
        };

        // bottom-anchor: when content is shorter than the viewport,
        // pad with empty lines so messages sit near the input box.
        // since indent_line pre-wraps to content_width, each line fits
        // within area.width so lines.len() equals the wrapped count
        let content_lines = lines.len().min(u16::MAX as usize) as u16;
        let visible = area.height;

        let padding = if content_lines < visible {
            (visible - content_lines) as usize
        } else {
            0
        };

        // compute per-message wrapped-line ranges for mouse hit testing
        // (done before padding/scroll so we work with original line indices).
        // all lines are pre-wrapped so each raw line index maps 1:1 to a row
        if !msg_line_starts.is_empty() && w > 0 {
            let total_raw = lines.len();
            let mut ranges = Vec::with_capacity(msg_line_starts.len());
            for (idx, &(msg_idx, start)) in msg_line_starts.iter().enumerate() {
                let end_raw = if idx + 1 < msg_line_starts.len() {
                    msg_line_starts[idx + 1].1
                } else {
                    total_raw
                };
                ranges.push(crate::app::MessageRowRange {
                    msg_idx,
                    start: padding as u16 + start as u16,
                    end: padding as u16 + end_raw as u16,
                });
            }
            *self.app.render_state.message_row_ranges.borrow_mut() = ranges;
        } else {
            self.app
                .render_state
                .message_row_ranges
                .borrow_mut()
                .clear();
        }

        let total_lines = content_lines + padding as u16;
        let max_scroll = total_lines.saturating_sub(visible);
        let effective_offset_u16 = effective_offset.min(u16::MAX as usize) as u16;
        let scroll = max_scroll.saturating_sub(effective_offset_u16);

        // expose scroll geometry for the status bar
        self.app.render_state.total_content_lines.set(total_lines);
        self.app.render_state.visible_area_height.set(visible);
        self.app.render_state.message_area.set(area);
        self.app.render_state.render_scroll.set(scroll);

        // compute image render areas based on scroll position
        let mut render_areas = Vec::new();
        if !image_placeholders.is_empty() && area.width > 0 {
            for (i, ph) in image_placeholders.iter().enumerate() {
                let y_before = img_y_positions[i];
                // skip the label line, image starts on the line after
                let img_y = y_before.saturating_add(1).saturating_sub(scroll);
                let img_height = IMAGE_HEIGHT.saturating_sub(1); // minus the label
                // check if visible
                if img_y < visible && img_y + img_height > 0 {
                    let visible_y = area.y + img_y;
                    let visible_h = img_height.min(visible.saturating_sub(img_y));
                    // indent 4 chars, leave some right margin
                    let img_x = area.x + 4;
                    let img_w = area.width.saturating_sub(8); // 4 left + 4 right margin
                    if img_w > 0 && visible_h > 0 {
                        render_areas.push(ImageRenderArea {
                            msg_idx: ph.msg_idx,
                            tc_idx: ph.tc_idx,
                            area: Rect::new(img_x, visible_y, img_w, visible_h),
                        });
                    }
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
            let line_idx = doc_y - padding;
            if line_idx >= lines.len() {
                break;
            }
            buf.set_line(
                area.x,
                area.y + screen_y as u16,
                &lines[line_idx],
                area.width,
            );
        }

        // overlay deferred cached content directly to the buffer.
        // during the lines build, cached content was replaced with empty
        // placeholders to avoid deep-cloning Line vecs. now we borrow
        // from the indented_cache and write directly, zero clones
        if !deferred_renders.is_empty() {
            let scroll_usize = scroll as usize;
            let visible_usize = area.height as usize;
            let cache = self.app.render_state.indented_cache.borrow();
            for dc in &deferred_renders {
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
        if in_scroll_mode && self.app.navigation.scroll_unit != crate::app::ScrollUnit::Block {
            apply_message_highlight(
                self.app,
                buf,
                area,
                scroll,
                &self.app.render_state.message_row_ranges.borrow(),
            );
        }
    }
}

/// height reserved for inline image rendering (in lines)
const IMAGE_HEIGHT: u16 = 12;

/// tracks where an image placeholder starts in the lines vec
struct ImagePlaceholder {
    msg_idx: usize,
    tc_idx: usize,
    /// line index in the lines vec where the placeholder starts
    line_idx: usize,
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

#[allow(clippy::too_many_arguments)]
fn render_message(
    app: &App,
    msg: &DisplayMessage,
    msg_idx: usize,
    lines: &mut Vec<Line<'_>>,
    sel: SelectionHint,
    image_placeholders: &mut Vec<ImagePlaceholder>,
    deferred: &mut Vec<DeferredCacheRender>,
    width: u16,
    all_blocks: &[CodeBlock],
) {
    // user and assistant messages have no label line.
    // user messages are distinguished by a subtle background.
    // only system messages get a text label.
    let is_user = matches!(msg.role, MessageRole::User);

    if matches!(msg.role, MessageRole::System) {
        let mut label_spans = Vec::new();
        if sel.selected {
            label_spans.push(Span::styled("▌ ", app.theme.selection_marker));
        }
        label_spans.push(Span::styled("system", app.theme.system_label));
        lines.push(Line::from(label_spans));
    } else if sel.selected {
        let mut hint_spans = vec![Span::styled("▌", app.theme.selection_marker)];
        if sel.is_cursor {
            let hint = if app.navigation.scroll_unit == crate::app::ScrollUnit::Block {
                " (y to copy block, b for messages)"
            } else if sel.has_visual {
                " (y to copy range)"
            } else {
                " (v to select, y to copy)"
            };
            hint_spans.push(Span::styled(hint, app.theme.scroll_hint));
        }
        lines.push(Line::from(hint_spans));
    }

    // thinking block
    if let Some(ref thinking) = msg.thinking {
        if msg.thinking_expanded {
            let cw = (width as usize).saturating_sub(1);
            for text_line in thinking.lines() {
                let styled = Line::styled(text_line.to_string(), app.theme.thinking);
                lines.extend(indent_line(styled, cw));
            }
        } else {
            let preview = thinking.lines().next().unwrap_or("...");
            let trimmed = if preview.chars().count() > 60 {
                let truncated: String = preview.chars().take(57).collect();
                format!("{truncated}...")
            } else {
                preview.to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(" 💭 ", app.theme.thinking),
                Span::styled(trimmed, app.theme.dim),
                Span::styled(" [ctrl+o]", app.theme.dim),
            ]));
        }
    }

    // main content (markdown rendered)
    if msg.queued {
        let dim = app.theme.dim;
        let cw = (width as usize).saturating_sub(1);
        for text_line in msg.content.lines() {
            let styled = Line::styled(text_line.to_string(), dim);
            lines.extend(indent_line(styled, cw));
        }
    } else if is_user {
        let w = width as usize;
        // 1-space indent, so content wraps within w-1 chars
        let content_width = w.saturating_sub(1);
        // blank padding line above
        lines.push(Line::from(Span::styled(
            " ".repeat(w),
            app.theme.user_msg_bg,
        )));
        for line in msg.content.lines() {
            for wrapped in wrap_text(line, content_width) {
                let text = format!(" {wrapped}");
                let pad = w.saturating_sub(text.chars().count());
                let padded = format!("{text}{}", " ".repeat(pad));
                lines.push(Line::from(Span::styled(padded, app.theme.user_msg_bg)));
            }
        }
        // blank padding line below
        lines.push(Line::from(Span::styled(
            " ".repeat(w),
            app.theme.user_msg_bg,
        )));
        // user message images
        for (img_idx, _) in msg.images.iter().enumerate() {
            image_placeholders.push(ImagePlaceholder {
                msg_idx,
                tc_idx: img_idx,
                line_idx: lines.len(),
            });
            lines.push(Line::from(vec![
                Span::styled("    📷 ", Style::default()),
                Span::styled("image", app.theme.image_label),
            ]));
            for _ in 1..IMAGE_HEIGHT {
                lines.push(Line::raw(""));
            }
        }
    } else {
        // determine if a code block in this message is selected
        let highlight_block = if app.interaction.mode == crate::app::AppMode::Scroll
            && app.navigation.scroll_unit == crate::app::ScrollUnit::Block
        {
            if let Some(sel) = app.navigation.selected_block {
                all_blocks
                    .get(sel)
                    .filter(|b| b.msg_idx == msg_idx)
                    .map(|_| {
                        all_blocks[..sel]
                            .iter()
                            .filter(|b| b.msg_idx == msg_idx)
                            .count()
                    })
            } else {
                None
            }
        } else {
            None
        };

        let content_width = (width as usize).saturating_sub(1);
        let highlight_range =
            highlight_block.and_then(|b| code_block_line_ranges(&msg.content).get(b).copied());

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
                let md_text = render_markdown_cached(app, &msg.content);
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
            let md_text = render_markdown_cached(app, &msg.content);
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

    // tool calls: group by batch, render as bordered boxes
    // skip running tools (they're shown in the live tool panels)
    let completed: Vec<(usize, &DisplayToolCall)> = msg
        .tool_calls
        .iter()
        .enumerate()
        .filter(|(_, tc)| tc.status != ToolCallStatus::Running)
        .collect();

    // group consecutive tools with the same batch
    let mut i = 0;
    while i < completed.len() {
        let batch = completed[i].1.batch;
        let group_start = i;
        while i < completed.len() && completed[i].1.batch == batch {
            i += 1;
        }
        let group = &completed[group_start..i];

        // collect image placeholders before rendering the group
        for &(tc_idx, tc) in group {
            if tc.image_data.is_some() {
                image_placeholders.push(ImagePlaceholder {
                    msg_idx,
                    tc_idx,
                    line_idx: lines.len(),
                });
            }
        }

        render_tool_box_group(
            &group.iter().map(|(_, tc)| *tc).collect::<Vec<_>>(),
            width,
            lines,
            &app.theme,
        );

        // after the boxes, render any image placeholders
        for &(_, tc) in group {
            if tc.image_data.is_some() {
                lines.push(Line::from(vec![
                    Span::styled("    📷 ", Style::default()),
                    Span::styled("image", app.theme.image_label),
                ]));
                for _ in 1..IMAGE_HEIGHT {
                    lines.push(Line::raw(""));
                }
            }
        }
    }

    // usage line (compact: total tokens + cost, with cache reuse + write ratios)
    // gated on show_usage_lines since the same info is in the status bar.
    // deferred while any tool is still running to avoid a "premature" usage
    // line appearing above the running tool panel
    let has_running_tool = msg
        .tool_calls
        .iter()
        .any(|tc| tc.status == ToolCallStatus::Running);
    if app.interaction.show_usage_lines
        && !has_running_tool
        && let Some(ref usage) = msg.usage
    {
        let total = usage.total_tokens();
        if total > TokenCount::ZERO {
            let mut parts = vec![format!(" {total}tok")];
            let total_input = usage.total_input_tokens();
            if total_input > TokenCount::ZERO {
                let reuse_pct = usage.cache_read_tokens.percent_of(total_input) as u32;
                parts.push(format!("reuse {reuse_pct}%"));
            }
            if usage.cache_write_tokens > TokenCount::ZERO {
                let write_pct = usage
                    .cache_write_tokens
                    .percent_of(usage.total_input_tokens()) as u32;
                parts.push(format!("write {write_pct}%"));
            }
            if let Some(c) = msg.cost {
                parts.push(format!("{c}"));
            }
            lines.push(Line::styled(parts.join(" | "), app.theme.usage));
        }
    }
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
/// to avoid re-scanning content with chars().count() every frame.
/// falls back to the indented cache or character-count estimation on miss.
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
    let cw = (width as usize).saturating_sub(1).max(1);
    let mut h = 1; // separator line

    // system label
    if matches!(msg.role, MessageRole::System) {
        h += 1;
    }

    // thinking
    if let Some(ref thinking) = msg.thinking {
        if msg.thinking_expanded {
            h += count_estimated_lines(thinking, cw);
        } else {
            h += 1;
        }
    }

    // content
    if matches!(msg.role, MessageRole::User) {
        h += 2; // top + bottom padding
        h += count_estimated_lines(&msg.content, cw);
        h += msg.images.len() * IMAGE_HEIGHT as usize;
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

/// estimate how many wrapped lines text occupies at a given width
fn count_estimated_lines(text: &str, width: usize) -> usize {
    #[cfg(test)]
    COUNT_ESTIMATED_LINES_CALLS.with(|c| c.set(c.get() + 1));

    if text.is_empty() || width == 0 {
        return 1;
    }
    text.lines()
        .map(|line| {
            let chars = line.chars().count();
            if chars == 0 { 1 } else { chars.div_ceil(width) }
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
fn render_markdown_cached(app: &App, source: &str) -> Text<'static> {
    if source.is_empty() {
        return Text::default();
    }

    if let Some(cached) = app
        .render_state
        .markdown_cache
        .borrow()
        .get(source)
        .cloned()
    {
        return cached;
    }

    let rendered = crate::markdown::render(source, &app.theme);
    app.render_state
        .markdown_cache
        .borrow_mut()
        .insert(source.to_string(), rendered.clone());
    rendered
}

fn render_streaming_markdown_cached(app: &App, source: &str) -> Text<'static> {
    if source.is_empty() {
        return Text::default();
    }

    // cache against the FULL stream buffer, not the visible (typewriter-truncated)
    // portion. this way typewriter ticks don't invalidate the cache: we only
    // re-parse when new deltas arrive
    let full = &app.stream.text;

    if let Some((cached_source, cached)) = app.render_state.stream_markdown_cache.borrow().as_ref()
        && cached_source == full
    {
        // cache hit: truncate the pre-rendered output to the visible char count
        return truncate_text(cached.clone(), source.chars().count());
    }

    let rendered = crate::markdown::render(full, &app.theme);
    *app.render_state.stream_markdown_cache.borrow_mut() =
        Some((full.to_string(), rendered.clone()));
    truncate_text(rendered, source.chars().count())
}

/// truncate a rendered Text to at most `max_chars` visible characters
fn truncate_text(text: Text<'static>, max_chars: usize) -> Text<'static> {
    let total: usize = text.lines.iter().map(|l| l.width()).sum();
    if total <= max_chars {
        return text;
    }

    let mut remaining = max_chars;
    let mut result = Vec::new();
    for line in text.lines {
        if remaining == 0 {
            break;
        }
        let line_w = line.width();
        if line_w <= remaining {
            remaining -= line_w;
            result.push(line);
        } else {
            // truncate this line's spans to fit
            let mut spans = Vec::new();
            let mut left = remaining;
            for span in line.spans {
                let sw = span.width();
                if sw <= left {
                    left -= sw;
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

// -- bordered tool boxes for completed tool calls --

/// indent for tool boxes (matches message content indent)
const BOX_INDENT: usize = 1;

/// minimum width per panel for side-by-side tool boxes
const MIN_TOOL_BOX_WIDTH: u16 = 30;

/// render a group of completed tool calls (same batch) as bordered boxes
fn render_tool_box_group(
    tools: &[&DisplayToolCall],
    total_width: u16,
    lines: &mut Vec<Line<'_>>,
    theme: &Theme,
) {
    let usable = total_width.saturating_sub(BOX_INDENT as u16);
    if usable < 8 || tools.is_empty() {
        return;
    }

    let n = tools.len();
    let side_by_side = n > 1 && usable / n as u16 >= MIN_TOOL_BOX_WIDTH;

    if side_by_side {
        render_side_by_side_boxes(tools, usable as usize, lines, theme);
    } else {
        for tool in tools {
            render_single_tool_box(tool, usable as usize, lines, theme);
        }
    }
}

/// render one completed tool as a bordered box
fn render_single_tool_box(
    tc: &DisplayToolCall,
    width: usize,
    lines: &mut Vec<Line<'_>>,
    theme: &Theme,
) {
    let (icon, colour) = tool_icon_colour(tc, theme);
    let border = Style::default().fg(colour);
    let dim = theme.dim;

    // title: " ✓ name "
    let title_text = format!(" {icon} {} ", tc.name);
    let title_chars = title_text.chars().count();
    // ┌─ + title + ─...─ + ┐  (width total)
    let fill = width.saturating_sub(title_chars + 3); // 3 = ┌─ + ┐

    // top border
    let indent = Span::raw(" ".repeat(BOX_INDENT));
    lines.push(Line::from(vec![
        indent.clone(),
        Span::styled("┌─", border),
        Span::styled(format!(" {icon} "), Style::default().fg(colour)),
        Span::styled(
            tc.name.clone(),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", border),
        Span::styled("─".repeat(fill), border),
        Span::styled("┐", border),
    ]));

    // content: summary line
    let inner = width.saturating_sub(4); // │ + space + space + │
    push_box_content_line(&tc.summary, inner, border, dim, &indent, lines);

    // content: output preview with diff colouring
    if let Some(ref output) = tc.output_preview {
        if tc.name == "edit" {
            // edit tool produces structured +/- diffs: render in side-by-side
            // columns when the inner panel is wide enough, otherwise inline
            for row in super::diff::render_diff(output, inner, theme) {
                let mut spans = vec![indent.clone(), Span::styled("│ ", border)];
                spans.extend(row.spans);
                spans.push(Span::styled(" │", border));
                lines.push(Line::from(spans));
            }
        } else {
            for text_line in output.lines() {
                push_box_content_line(
                    text_line,
                    inner,
                    border,
                    diff_line_style(text_line, theme),
                    &indent,
                    lines,
                );
            }
        }
    }

    // bottom border
    lines.push(Line::from(vec![
        indent,
        Span::styled("└", border),
        Span::styled("─".repeat(width.saturating_sub(2)), border),
        Span::styled("┘", border),
    ]));
}

/// render parallel tools side-by-side in a shared bordered box
fn render_side_by_side_boxes(
    tools: &[&DisplayToolCall],
    width: usize,
    lines: &mut Vec<Line<'_>>,
    theme: &Theme,
) {
    let n = tools.len();
    // each panel width (including its borders): divide evenly
    // total = panel_w * n + (n-1) separators... but we share borders
    // shared layout: ┌─ a ─┬─ b ─┐ = width total
    // each panel inner = (width - n - 1) / n
    let inner_total = width.saturating_sub(n + 1); // n+1 border chars (│ or ┬/┴)
    let panel_inner = inner_total / n;
    let remainder = inner_total % n;

    let indent = Span::raw(" ".repeat(BOX_INDENT));

    // determine border colour per panel
    let colours: Vec<Color> = tools
        .iter()
        .map(|tc| tool_icon_colour(tc, theme).1)
        .collect();

    // -- top border --
    let mut top_spans = vec![indent.clone()];
    for (i, tc) in tools.iter().enumerate() {
        let (icon, colour) = tool_icon_colour(tc, theme);
        let border = Style::default().fg(colour);
        let corner = if i == 0 { "┌─" } else { "┬─" };
        let title = format!(" {icon} {} ", tc.name);
        let title_chars = title.chars().count();
        let pw = panel_inner + if i < remainder { 1 } else { 0 };
        let fill = pw.saturating_sub(title_chars + 1); // 1 for the ─ after corner

        top_spans.push(Span::styled(corner, border));
        top_spans.push(Span::styled(
            format!(" {icon} "),
            Style::default().fg(colour),
        ));
        top_spans.push(Span::styled(
            tc.name.clone(),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ));
        top_spans.push(Span::styled(" ", border));
        top_spans.push(Span::styled("─".repeat(fill), border));
    }
    top_spans.push(Span::styled(
        "┐",
        Style::default().fg(*colours.last().unwrap_or(&Color::DarkGray)),
    ));
    lines.push(Line::from(top_spans));

    // -- content rows: max of content heights across panels --
    // pre-wrap content per panel to fit panel width
    let panel_contents: Vec<Vec<String>> = tools
        .iter()
        .enumerate()
        .map(|(i, tc)| {
            let pw = panel_inner + if i < remainder { 1 } else { 0 };
            let text_width = pw.saturating_sub(1); // reserve 1 for leading space
            let mut content = Vec::new();
            for wrapped in wrap_text(&tc.summary, text_width) {
                content.push(wrapped);
            }
            if let Some(ref output) = tc.output_preview {
                for line in output.lines() {
                    for wrapped in wrap_text(line, text_width) {
                        content.push(wrapped);
                    }
                }
            }
            content
        })
        .collect();

    let max_rows = panel_contents.iter().map(|c| c.len()).max().unwrap_or(0);

    for row in 0..max_rows {
        let mut spans = vec![indent.clone()];
        for (i, content) in panel_contents.iter().enumerate() {
            let pw = panel_inner + if i < remainder { 1 } else { 0 };
            let border = Style::default().fg(colours[i]);
            spans.push(Span::styled("│", border));

            let text = content.get(row).map(|s| s.as_str()).unwrap_or("");
            let style = diff_line_style(text, theme);
            let used = text.chars().count() + 1; // +1 for leading space
            let pad = pw.saturating_sub(used);
            spans.push(Span::styled(format!(" {text}"), style));
            spans.push(Span::raw(" ".repeat(pad)));
        }
        spans.push(Span::styled(
            "│",
            Style::default().fg(*colours.last().unwrap_or(&Color::DarkGray)),
        ));
        lines.push(Line::from(spans));
    }

    // -- bottom border --
    let mut bot_spans = vec![indent];
    for (i, _) in tools.iter().enumerate() {
        let border = Style::default().fg(colours[i]);
        let pw = panel_inner + if i < remainder { 1 } else { 0 };
        let corner = if i == 0 { "└" } else { "┴" };
        bot_spans.push(Span::styled(corner, border));
        bot_spans.push(Span::styled("─".repeat(pw), border));
    }
    bot_spans.push(Span::styled(
        "┘",
        Style::default().fg(*colours.last().unwrap_or(&Color::DarkGray)),
    ));
    lines.push(Line::from(bot_spans));
}

/// push a styled content line inside a bordered box, wrapping if needed
fn push_box_content_line<'a>(
    text: &str,
    inner_width: usize,
    border: Style,
    style: Style,
    indent: &Span<'a>,
    lines: &mut Vec<Line<'a>>,
) {
    for wrapped in wrap_text(text, inner_width) {
        let pad = inner_width.saturating_sub(wrapped.chars().count());
        lines.push(Line::from(vec![
            indent.clone(),
            Span::styled("│ ", border),
            Span::styled(wrapped, style),
            Span::raw(" ".repeat(pad)),
            Span::styled(" │", border),
        ]));
    }
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
    if line.width() <= content_width {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(line.spans);
        return vec![Line::from(spans).style(line_style)];
    }

    // walk spans character by character, splitting at content_width
    let mut result: Vec<Line<'a>> = Vec::new();
    let mut current: Vec<Span<'a>> = Vec::new();
    let mut current_width: usize = 0;

    for span in line.spans {
        let style = span.style;
        let text: &str = &span.content;
        let mut seg_start = 0;

        for (byte_pos, ch) in text.char_indices() {
            let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if current_width + ch_w > content_width && current_width > 0 {
                // flush the segment up to this point
                if byte_pos > seg_start {
                    current.push(Span::styled(text[seg_start..byte_pos].to_string(), style));
                }
                result.push(Line::from(std::mem::take(&mut current)).style(line_style));
                current_width = 0;
                seg_start = byte_pos;
            }
            current_width += ch_w;
        }

        // remainder of span
        if seg_start < text.len() {
            current.push(Span::styled(text[seg_start..].to_string(), style));
        }
    }

    if !current.is_empty() {
        result.push(Line::from(current).style(line_style));
    }

    // prepend the indent to every line
    for line in &mut result {
        line.spans.insert(0, Span::raw(" "));
    }

    result
}

/// wrap text to fit within `width` chars, breaking at spaces first,
/// then character-wise for words longer than the width
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    if text.chars().count() <= width {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len: usize = 0;

    for word in text.split(' ') {
        let word_len = word.chars().count();

        if word_len > width {
            // push current line first
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_len = 0;
            }
            // character-wrap the long word
            let mut remaining = word;
            while !remaining.is_empty() {
                let end = remaining
                    .char_indices()
                    .nth(width)
                    .map_or(remaining.len(), |(i, _)| i);
                let chunk = &remaining[..end];
                remaining = &remaining[end..];
                if remaining.is_empty() {
                    // last chunk becomes current line so next word can join
                    current = chunk.to_string();
                    current_len = chunk.chars().count();
                } else {
                    lines.push(chunk.to_string());
                }
            }
        } else if current.is_empty() {
            current = word.to_string();
            current_len = word_len;
        } else if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
            current_len = word_len;
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

/// get icon and colour for a completed tool call
fn tool_icon_colour(tc: &DisplayToolCall, theme: &Theme) -> (&'static str, Color) {
    match tc.status {
        ToolCallStatus::Done => ("✓", theme.tool_done.fg.unwrap_or(Color::Green)),
        ToolCallStatus::Error => ("✗", theme.tool_error.fg.unwrap_or(Color::Red)),
        ToolCallStatus::Running => ("⣾", theme.tool_running.fg.unwrap_or(Color::Cyan)),
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

/// compute rendered-line ranges for each fenced code block in source
/// returns (start, end) where start is inclusive, end is exclusive
fn code_block_line_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut in_block = false;
    let mut rendered_idx = 0;
    let mut block_start = 0;

    for line in source.lines() {
        if line.starts_with("```") {
            if in_block {
                ranges.push((block_start, rendered_idx));
                in_block = false;
            } else {
                block_start = rendered_idx;
                in_block = true;
            }
            // fence markers are consumed, not rendered
            continue;
        }
        rendered_idx += 1;
    }

    if in_block && rendered_idx > block_start {
        ranges.push((block_start, rendered_idx));
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
mod tests {
    use super::*;
    use mush_ai::types::{Dollars, Usage};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_app(app: &App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(MessageList::new(app), area);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn write_lines_to_buf_renders_at_position() {
        let lines = vec![Line::raw("alpha"), Line::raw("beta"), Line::raw("gamma")];
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);

        let written = write_lines_to_buf(&lines, &mut buf, area, 0);

        assert_eq!(written, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("alpha"));
        assert!(content.contains("beta"));
        assert!(content.contains("gamma"));
    }

    #[test]
    fn write_lines_to_buf_clips_to_area_height() {
        let lines = vec![Line::raw("alpha"), Line::raw("beta"), Line::raw("gamma")];
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);

        let written = write_lines_to_buf(&lines, &mut buf, area, 0);

        assert_eq!(written, 2);
        let content = buffer_to_string(&buf);
        assert!(content.contains("alpha"));
        assert!(content.contains("beta"));
        assert!(!content.contains("gamma"));
    }

    #[test]
    fn write_lines_to_buf_starts_at_y_offset() {
        let lines = vec![Line::raw("alpha"), Line::raw("beta")];
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);

        let written = write_lines_to_buf(&lines, &mut buf, area, 1);

        assert_eq!(written, 2);
        let row0: String = (0..20)
            .map(|x| buf[(x, 0u16)].symbol().to_string())
            .collect();
        let row1: String = (0..20)
            .map(|x| buf[(x, 1u16)].symbol().to_string())
            .collect();
        assert!(row0.trim().is_empty(), "row 0 should be empty");
        assert!(
            row1.starts_with("alpha"),
            "row 1 should have alpha, got '{row1}'"
        );
    }

    #[test]
    fn empty_app_renders() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let buf = render_app(&app, 40, 10);
        // should be mostly empty
        let content = buffer_to_string(&buf);
        assert!(content.trim().is_empty());
    }

    #[test]
    fn long_conversation_renders_without_clone() {
        // regression: the render path used to clone the entire Text just to
        // count wrapped lines. verify that a multi-message conversation
        // still renders correctly with the lines.len() optimisation
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        for i in 0..20 {
            let role = if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            app.messages.push(DisplayMessage::new(
                role,
                format!("message number {i} with some text"),
            ));
        }
        let buf = render_app(&app, 60, 30);
        let content = buffer_to_string(&buf);
        // last message should be visible near the bottom
        assert!(
            content.contains("message number 19"),
            "last message should be visible"
        );
        // content should be bottom-anchored (no excess padding at top)
        // find first non-empty line
        let first_content_line = content
            .lines()
            .position(|l| !l.trim().is_empty())
            .unwrap_or(0);
        // with 20 messages and 30 lines visible, content should start near the top
        assert!(
            first_content_line < 5,
            "content should start near top, but first content at line {first_content_line}"
        );
    }

    #[test]
    fn user_message_renders() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_user_message("hello world");
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("hello world"));
    }

    #[test]
    fn assistant_message_renders() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("i can help");
        app.finish_streaming(None, None);
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("i can help"));
    }

    #[test]
    fn streaming_shows_partial_text() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("partial");
        // tick several times to let typewriter catch up
        for _ in 0..10 {
            app.tick();
        }
        // don't finish - still streaming
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("partial"));
    }

    #[test]
    fn streaming_markdown_is_reused_when_visible_text_is_unchanged() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("**cached**");
        for _ in 0..10 {
            app.tick();
        }

        crate::markdown::reset_render_call_count();
        let _ = render_app(&app, 40, 10);
        let _ = render_app(&app, 40, 10);

        assert_eq!(crate::markdown::render_call_count(), 1);
    }

    #[test]
    fn usage_line_shows_reuse_and_write_ratios() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.show_usage_lines = true;
        app.messages.push(DisplayMessage {
            usage: Some(Usage {
                input_tokens: TokenCount::new(100),
                output_tokens: TokenCount::new(20),
                cache_read_tokens: TokenCount::new(150),
                cache_write_tokens: TokenCount::new(50),
            }),
            cost: Some(Dollars::new(0.0012)),
            ..DisplayMessage::new(MessageRole::Assistant, "done")
        });

        let buf = render_app(&app, 70, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("320tok"));
        // reuse = cache_read / total_input = 150/300 = 50%
        assert!(content.contains("reuse 50%"));
        assert!(content.contains("write 16%"));
        assert!(content.contains("$0.0012"));
    }

    #[test]
    fn usage_line_reuse_reflects_cache_bust() {
        // during a bust: cache_read drops, cache_write spikes.
        // old formula (cache_read / (cache_read + input)) would show 99%,
        // new formula (cache_read / total_input) correctly shows ~4%
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.show_usage_lines = true;
        app.messages.push(DisplayMessage {
            usage: Some(Usage {
                input_tokens: TokenCount::new(50),
                output_tokens: TokenCount::new(500),
                cache_read_tokens: TokenCount::new(5_000),
                cache_write_tokens: TokenCount::new(105_000),
            }),
            ..DisplayMessage::new(MessageRole::Assistant, "done")
        });

        let buf = render_app(&app, 70, 10);
        let content = buffer_to_string(&buf);
        // 5000 / (5000 + 105000 + 50) = 4%, NOT 99%
        assert!(
            content.contains("reuse 4%"),
            "during a cache bust reuse should reflect total input, got: {content}"
        );
    }

    #[test]
    fn usage_line_hidden_by_default_shown_when_enabled() {
        // default: show_usage_lines = false, usage line must not render
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            usage: Some(Usage {
                input_tokens: TokenCount::new(100),
                output_tokens: TokenCount::new(20),
                cache_read_tokens: TokenCount::new(150),
                cache_write_tokens: TokenCount::new(50),
            }),
            cost: Some(Dollars::new(0.0012)),
            ..DisplayMessage::new(MessageRole::Assistant, "done")
        });

        let hidden = buffer_to_string(&render_app(&app, 70, 10));
        assert!(
            !hidden.contains("320tok"),
            "usage line should be hidden by default, got: {hidden}"
        );
        assert!(
            !hidden.contains("reuse"),
            "usage line should be hidden by default, got: {hidden}"
        );

        // flipping the toggle shows the usage line
        app.interaction.show_usage_lines = true;
        let shown = buffer_to_string(&render_app(&app, 70, 10));
        assert!(
            shown.contains("320tok"),
            "usage line should render when toggled on, got: {shown}"
        );
    }

    #[test]
    fn usage_line_deferred_while_tool_running() {
        // MessageEnd attaches usage before tools complete; rendering the usage
        // line immediately produces a "premature" line while the tool is still
        // running in the active_tools panel. the usage line should stay hidden
        // until every tool on the message finishes
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.show_usage_lines = true;
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "ls -la".into(),
                status: ToolCallStatus::Running,
                output_preview: None,
                image_data: None,
                batch: 1,
            }],
            usage: Some(Usage {
                input_tokens: TokenCount::new(100),
                output_tokens: TokenCount::new(20),
                cache_read_tokens: TokenCount::new(150),
                cache_write_tokens: TokenCount::new(50),
            }),
            cost: Some(Dollars::new(0.0012)),
            ..DisplayMessage::new(MessageRole::Assistant, "running now")
        });

        let running = buffer_to_string(&render_app(&app, 70, 12));
        assert!(
            !running.contains("320tok"),
            "usage line should be deferred while a tool is running, got: {running}"
        );

        // once the tool finishes, the usage line appears
        app.messages[0].tool_calls[0].status = ToolCallStatus::Done;
        app.messages[0].tool_calls[0].output_preview = Some("ok".into());
        let done = buffer_to_string(&render_app(&app, 70, 12));
        assert!(
            done.contains("320tok"),
            "usage line should render after tools complete, got: {done}"
        );
    }

    #[test]
    fn tool_calls_render() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![
                crate::app::DisplayToolCall {
                    name: "bash".into(),
                    summary: "ls -la".into(),
                    status: ToolCallStatus::Done,
                    output_preview: Some("file1.txt\nfile2.txt".into()),
                    image_data: None,
                    batch: 1,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "src/main.rs".into(),
                    status: ToolCallStatus::Error,
                    output_preview: None,
                    image_data: None,
                    batch: 2,
                },
            ],
            ..DisplayMessage::new(MessageRole::Assistant, "let me check")
        });
        let buf = render_app(&app, 50, 15);
        let content = buffer_to_string(&buf);
        assert!(content.contains("bash"));
        assert!(content.contains("read"));
    }

    #[test]
    fn edit_tool_renders_side_by_side_diff_when_wide() {
        // edit tool output contains +/- diff lines. at wide widths the box
        // should render a side-by-side column layout with `│` separating
        // removed (left) from added (right)
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "edit".into(),
                summary: "src/main.rs".into(),
                status: ToolCallStatus::Done,
                output_preview: Some("- println!(\"hello\")\n+ println!(\"world\")".into()),
                image_data: None,
                batch: 1,
            }],
            ..DisplayMessage::new(MessageRole::Assistant, "patching")
        });
        let buf = render_app(&app, 120, 10);
        let content = buffer_to_string(&buf);
        // both versions visible on the same row implies side-by-side
        let row_with_both = content
            .lines()
            .find(|row| row.contains("println!(\"hello\")") && row.contains("println!(\"world\")"));
        assert!(
            row_with_both.is_some(),
            "expected side-by-side row with both versions, got:\n{content}"
        );
    }

    #[test]
    fn edit_tool_renders_inline_diff_when_narrow() {
        // at narrow widths the diff falls back to inline: +/- lines stack
        // vertically like the current default
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "edit".into(),
                summary: "src/main.rs".into(),
                status: ToolCallStatus::Done,
                output_preview: Some("- println!(\"hello\")\n+ println!(\"world\")".into()),
                image_data: None,
                batch: 1,
            }],
            ..DisplayMessage::new(MessageRole::Assistant, "patching")
        });
        let buf = render_app(&app, 60, 10);
        let content = buffer_to_string(&buf);
        // no single row should have both sides: `hello` and `world` on different lines
        let any_row_with_both = content
            .lines()
            .any(|row| row.contains("println!(\"hello\")") && row.contains("println!(\"world\")"));
        assert!(
            !any_row_with_both,
            "narrow mode should render inline, not side-by-side; got:\n{content}"
        );
        assert!(content.contains("hello"));
        assert!(content.contains("world"));
    }

    #[test]
    fn thinking_shows_collapsed() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            thinking: Some(
                "first i need to consider the question deeply and think about it".into(),
            ),
            ..DisplayMessage::new(MessageRole::Assistant, "the answer is 42")
        });
        let buf = render_app(&app, 60, 10);
        let content = buffer_to_string(&buf);
        // should show the thinking emoji indicator
        assert!(content.contains("💭"));
    }

    #[test]
    fn expanded_thinking_uses_thinking_style() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            thinking: Some("deep thoughts here".into()),
            thinking_expanded: true,
            ..DisplayMessage::new(MessageRole::Assistant, "the answer")
        });
        let buf = render_app(&app, 60, 10);

        // find a cell containing thinking text and check its colour
        let thinking_fg = app.theme.thinking.fg;
        let dim_fg = app.theme.dim.fg;
        assert_ne!(thinking_fg, dim_fg, "thinking and dim should differ");

        let thinking_cell = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                let cell = &buf[(x, y)];
                cell.symbol() == "d" && {
                    // "deep" starts with d
                    let next = &buf[(x + 1, y)];
                    next.symbol() == "e"
                }
            });
        let (x, y) = thinking_cell.expect("should find thinking text 'de' in buffer");
        let cell = &buf[(x, y)];
        assert_eq!(
            cell.fg,
            thinking_fg.unwrap(),
            "thinking text should use thinking style, not dim"
        );
    }

    #[test]
    fn image_reserves_space_and_produces_render_area() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "read".into(),
                summary: "photo.png".into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: Some(vec![0u8; 100]), // dummy bytes
                batch: 1,
            }],
            ..DisplayMessage::new(MessageRole::Assistant, "here is the image")
        });
        let buf = render_app(&app, 60, 30);
        let content = buffer_to_string(&buf);
        // should show the image label
        assert!(content.contains("📷"));
        assert!(content.contains("image"));
        // should have produced a render area
        let areas = app.render_state.image_render_areas.borrow();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].msg_idx, 0);
        assert_eq!(areas[0].tc_idx, 0);
        // area should have reasonable dimensions
        assert!(areas[0].area.height > 0);
        assert!(areas[0].area.width > 0);
    }

    #[test]
    fn completed_tool_renders_bordered_box() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "cargo test".into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: None,
                batch: 1,
            }],
            ..DisplayMessage::new(MessageRole::Assistant, "checking")
        });
        let buf = render_app(&app, 50, 10);
        let content = buffer_to_string(&buf);
        // bordered box with green tick and tool name in title
        assert!(content.contains("┌"), "missing top-left corner");
        assert!(content.contains("✓"), "missing tick");
        assert!(content.contains("bash"), "missing tool name");
        assert!(content.contains("cargo test"), "missing summary");
        assert!(content.contains("└"), "missing bottom-left corner");
    }

    #[test]
    fn failed_tool_renders_red_bordered_box() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "cargo check".into(),
                status: ToolCallStatus::Error,
                output_preview: Some("error[E0063]: missing field".into()),
                image_data: None,
                batch: 1,
            }],
            ..DisplayMessage::new(MessageRole::Assistant, "checking")
        });
        let buf = render_app(&app, 60, 12);
        let content = buffer_to_string(&buf);
        assert!(content.contains("✗"), "missing cross");
        assert!(content.contains("bash"), "missing tool name");
        assert!(content.contains("error"), "missing error output");
    }

    #[test]
    fn parallel_tools_render_side_by_side() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "a.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "b.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1, // same batch = parallel
                },
            ],
            ..DisplayMessage::new(MessageRole::Assistant, "reading")
        });
        // 80 wide: each panel gets ~39 cols, > MIN_TOOL_BOX_WIDTH (30)
        let buf = render_app(&app, 80, 10);
        let content = buffer_to_string(&buf);
        // side-by-side uses ┬ as a junction between panels
        assert!(
            content.contains("┬"),
            "missing top junction (not side-by-side)"
        );
        assert!(content.contains("a.rs"), "missing first tool summary");
        assert!(content.contains("b.rs"), "missing second tool summary");
    }

    #[test]
    fn side_by_side_box_lines_have_equal_width() {
        // regression: content rows were 1 char wider per panel than borders
        // because the leading space in " {text}" wasn't accounted for in padding
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "file.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: Some("L1: hello\nL2: world".into()),
                    image_data: None,
                    batch: 1,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "other.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: Some("L1: foo\nL2: bar".into()),
                    image_data: None,
                    batch: 1,
                },
            ],
            ..DisplayMessage::new(MessageRole::Assistant, "reading files")
        });
        let buf = render_app(&app, 100, 20);
        let content = buffer_to_string(&buf);

        // find lines that belong to the box (contain box-drawing chars)
        let box_lines: Vec<&str> = content
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with('┌') || t.starts_with('│') || t.starts_with('└')
            })
            .collect();

        assert!(
            box_lines.len() >= 3,
            "expected at least top + content + bottom, got {}",
            box_lines.len()
        );

        // find the char-column of the rightmost non-whitespace on each line
        let right_edges: Vec<usize> = box_lines
            .iter()
            .map(|l| {
                l.chars()
                    .collect::<Vec<_>>()
                    .iter()
                    .rposition(|c| !c.is_whitespace())
                    .map(|pos| pos + 1)
                    .unwrap_or(0)
            })
            .collect();

        let first = right_edges[0];
        for (i, &edge) in right_edges.iter().enumerate() {
            assert_eq!(
                edge, first,
                "box line {i} right edge at col {edge} != expected {first}\n  line: {:?}\n  all edges: {right_edges:?}",
                box_lines[i]
            );
        }
    }

    #[test]
    fn selected_code_block_gets_highlight() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "here:\n```bash\nrm -rf mything\n```\nand:\n```python\nprint(42)\n```",
        ));
        // enter scroll mode with block selected
        app.interaction.mode = crate::app::AppMode::Scroll;
        app.navigation.scroll_unit = crate::app::ScrollUnit::Block;
        app.navigation.selected_block = Some(0); // first block (bash)

        let buf = render_app(&app, 60, 20);

        // find the cell that renders "rm" and check it has a bg colour
        let mut found_highlight = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                if cell.symbol() == "r" {
                    // check the next cell is "m" to confirm it's "rm"
                    if x + 1 < buf.area.width && buf[(x + 1, y)].symbol() == "m" {
                        // selected block should have a non-default background
                        if cell.bg != Color::Reset {
                            found_highlight = true;
                        }
                    }
                }
            }
        }
        assert!(
            found_highlight,
            "selected code block should have a background highlight"
        );
    }

    #[test]
    fn selected_message_in_message_mode_gets_highlight() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages
            .push(DisplayMessage::new(MessageRole::Assistant, "hello world"));
        app.messages
            .push(DisplayMessage::new(MessageRole::User, "second message"));
        app.interaction.mode = crate::app::AppMode::Scroll;
        app.navigation.scroll_unit = crate::app::ScrollUnit::Message;
        app.navigation.selected_message = Some(0); // first message

        let buf = render_app(&app, 60, 20);

        // check that the "h" of "hello" has a non-default bg
        let mut found_highlight = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                if cell.symbol() == "h"
                    && x + 1 < buf.area.width
                    && buf[(x + 1, y)].symbol() == "e"
                    && cell.bg != Color::Reset
                {
                    found_highlight = true;
                }
            }
        }
        assert!(
            found_highlight,
            "selected message should have a background highlight"
        );
    }

    #[test]
    fn parallel_tools_stack_when_narrow() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "a.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "b.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1,
                },
            ],
            ..DisplayMessage::new(MessageRole::Assistant, "reading")
        });
        // 40 wide: each panel would be ~19 cols, < MIN_TOOL_BOX_WIDTH (30)
        let buf = render_app(&app, 40, 12);
        let content = buffer_to_string(&buf);
        // stacked: no junction, two separate boxes
        assert!(!content.contains("┬"), "should not be side-by-side");
        assert!(content.contains("a.rs"), "missing first tool");
        assert!(content.contains("b.rs"), "missing second tool");
    }

    #[test]
    fn error_tool_box_has_red_border() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "read".into(),
                summary: "missing.rs".into(),
                status: ToolCallStatus::Error,
                output_preview: Some("file not found".into()),
                image_data: None,
                batch: 1,
            }],
            ..DisplayMessage::new(MessageRole::Assistant, "reading")
        });
        let buf = render_app(&app, 50, 10);
        // check that the top-left corner cell has red foreground
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                if cell.symbol() == "┌" {
                    assert_eq!(
                        cell.fg,
                        Color::Red,
                        "border should be red for error tool at ({x}, {y})"
                    );
                    return;
                }
            }
        }
        panic!("no ┌ found in rendered output");
    }

    #[test]
    fn wrap_text_short_line_unchanged() {
        assert_eq!(wrap_text("hello", 20), vec!["hello"]);
    }

    #[test]
    fn wrap_text_breaks_at_word_boundary() {
        assert_eq!(wrap_text("hello world foo", 11), vec!["hello world", "foo"]);
    }

    #[test]
    fn wrap_text_long_word_char_wraps() {
        assert_eq!(
            wrap_text("/very/long/path/name", 10),
            vec!["/very/long", "/path/name"]
        );
    }

    #[test]
    fn wrap_text_empty() {
        assert_eq!(wrap_text("", 10), vec![""]);
    }

    #[test]
    fn tool_box_wraps_long_summary() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let long_path = "/home/rowan/dev/mush/crates/mush-tui/src/widgets/message_list.rs";
        app.messages.push(DisplayMessage {
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "read".into(),
                summary: long_path.into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: None,
                batch: 1,
            }],
            ..DisplayMessage::new(MessageRole::Assistant, "reading")
        });
        // narrow box: path won't fit on one line
        let buf = render_app(&app, 40, 12);
        let content = buffer_to_string(&buf);
        // full path should be visible (wrapped, not truncated)
        assert!(
            content.contains("message_list.rs"),
            "path end should be visible after wrapping"
        );
        // no ellipsis
        assert!(!content.contains("…"), "should wrap, not truncate");
    }

    /// helper: convert buffer to string for assertions
    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                s.push_str(cell.symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn wrapped_assistant_lines_keep_indent() {
        // 20 chars wide, message longer than 19 (content width = width - 1 indent)
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "this line is way too long to fit in twenty characters",
        ));
        let buf = render_app(&app, 20, 10);
        let text = buffer_to_string(&buf);
        // every non-empty content line should start with a space (the indent)
        for line in text.lines() {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            assert!(
                trimmed.starts_with(' '),
                "wrapped line missing indent: {trimmed:?}"
            );
        }
    }

    #[test]
    fn streaming_typewriter_ticks_dont_reparse_markdown() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("hello **world** and some more text here");

        // advance typewriter so some text is visible
        app.tick();

        crate::markdown::reset_render_call_count();

        // first render: cache miss, parses markdown once
        render_app(&app, 60, 20);
        let first_count = crate::markdown::render_call_count();
        assert!(first_count > 0, "should parse at least once");

        // advance typewriter (changes visible text) and render again
        for _ in 0..5 {
            app.tick();
        }
        render_app(&app, 60, 20);
        let second_count = crate::markdown::render_call_count();

        // should not have re-parsed: the full buffer hasn't changed
        assert_eq!(
            first_count, second_count,
            "typewriter ticks should not cause markdown re-parsing"
        );
    }

    #[test]
    fn offscreen_messages_skip_markdown_render() {
        // with viewport-aware rendering, messages above the visible area
        // should not trigger markdown rendering or indent_line allocations
        crate::markdown::reset_render_call_count();
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        for i in 0..100 {
            let role = if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            app.messages.push(DisplayMessage::new(
                role,
                format!("message {i} with enough text to be a real message"),
            ));
        }

        // render in a small viewport (10 lines visible)
        render_app(&app, 60, 10);
        let first_renders = crate::markdown::render_call_count();

        // with 100 messages and only 10 visible lines, most messages
        // are off-screen. we should render FAR fewer than 50 assistant
        // messages worth of markdown (50 = half of 100 messages)
        assert!(
            first_renders < 20,
            "expected fewer than 20 markdown renders for 10-line viewport, got {first_renders}"
        );

        // the last message should still be visible
        let buf = render_app(&app, 60, 10);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("message 99"),
            "last message should be visible in viewport"
        );
    }

    #[test]
    fn stable_messages_reuse_cached_indented_lines() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        for i in 0..10 {
            app.messages.push(DisplayMessage::new(
                MessageRole::Assistant,
                format!("message {i} with **markdown** and `code`"),
            ));
        }

        // first render populates the cache
        reset_indent_line_call_count();
        render_app(&app, 60, 40);
        let first_calls = indent_line_call_count();
        assert!(first_calls > 0, "first render should call indent_line");

        // second render at same width should serve from cache
        reset_indent_line_call_count();
        render_app(&app, 60, 40);
        let second_calls = indent_line_call_count();
        assert_eq!(
            second_calls, 0,
            "second render should use cached indented lines, got {second_calls} calls"
        );
    }

    #[test]
    fn indented_cache_invalidates_on_width_change() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        for i in 0..5 {
            app.messages.push(DisplayMessage::new(
                MessageRole::Assistant,
                format!("message {i} with some text"),
            ));
        }

        // first render at width 60
        render_app(&app, 60, 20);

        // render at different width should recompute
        reset_indent_line_call_count();
        render_app(&app, 80, 20);
        let calls = indent_line_call_count();
        assert!(
            calls > 0,
            "width change should invalidate cache, got {calls} calls"
        );
    }

    #[test]
    fn height_cache_avoids_recomputation() {
        let render_state = crate::app_state::RenderState::new();
        let msg = DisplayMessage::new(
            MessageRole::User,
            "hello world this is some content for height estimation",
        );

        reset_count_estimated_lines_calls();
        let h1 = estimate_message_height(&msg, 0, 60, &render_state);
        let first_calls = count_estimated_lines_call_count();
        assert!(first_calls > 0, "should compute on first call");

        reset_count_estimated_lines_calls();
        let h2 = estimate_message_height(&msg, 0, 60, &render_state);
        let second_calls = count_estimated_lines_call_count();

        assert_eq!(h1, h2, "cached height should match");
        assert_eq!(second_calls, 0, "should use cache on second call");
    }

    #[test]
    fn height_cache_invalidates_on_content_change() {
        let render_state = crate::app_state::RenderState::new();
        let msg = DisplayMessage::new(MessageRole::User, "short");
        estimate_message_height(&msg, 0, 60, &render_state);

        // different content length invalidates and recomputes
        let msg2 = DisplayMessage::new(MessageRole::User, "different length content");
        reset_count_estimated_lines_calls();
        estimate_message_height(&msg2, 0, 60, &render_state);
        let calls = count_estimated_lines_call_count();

        assert!(calls > 0, "content change should recompute");
    }

    #[test]
    fn height_cache_invalidates_on_width_change() {
        let render_state = crate::app_state::RenderState::new();
        let msg = DisplayMessage::new(
            MessageRole::User,
            "some content that might wrap differently at different widths",
        );
        estimate_message_height(&msg, 0, 60, &render_state);

        reset_count_estimated_lines_calls();
        estimate_message_height(&msg, 0, 80, &render_state);
        let calls = count_estimated_lines_call_count();

        assert!(calls > 0, "width change should recompute");
    }

    #[test]
    fn content_hash_cached_across_renders() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "hello world with some **markdown**",
        ));

        // first render computes the hash
        render_app(&app, 60, 20);
        reset_content_hash_calls();

        // second render should reuse cached hash
        render_app(&app, 60, 20);
        let calls = content_hash_call_count();
        assert_eq!(
            calls, 0,
            "second render should use cached hash, got {calls} calls"
        );
    }

    #[test]
    fn scroll_position_stable_when_content_grows() {
        // bug: when scrolled up and new content arrives at the bottom,
        // the viewport shifts because scroll_offset is "from bottom" but
        // max_scroll increases. the view should stay pinned
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        // fill with enough messages to exceed viewport
        for i in 0..20 {
            app.messages.push(DisplayMessage::new(
                MessageRole::Assistant,
                format!("message number {i} with some content"),
            ));
        }

        // scroll up
        app.scroll_offset = 15;

        // first render to establish baseline
        render_app(&app, 60, 20);
        let scroll_before = app.render_state.render_scroll.get();
        assert!(scroll_before > 0, "should be scrolled");

        // simulate new content arriving (streaming adds a message)
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "new streaming content that just arrived",
        ));

        // second render: scroll position should stay the same
        render_app(&app, 60, 20);
        let scroll_after = app.render_state.render_scroll.get();

        assert_eq!(
            scroll_before, scroll_after,
            "viewport should stay pinned when content grows while scrolled up \
             (before={scroll_before}, after={scroll_after})"
        );
    }

    #[test]
    fn scroll_compensation_resets_at_bottom() {
        // when user scrolls back to bottom (scroll_offset=0), compensation
        // should reset so future scrolling starts fresh
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        for i in 0..20 {
            app.messages.push(DisplayMessage::new(
                MessageRole::Assistant,
                format!("message {i}"),
            ));
        }

        // scroll up, render, add content, render (accumulates compensation)
        app.scroll_offset = 10;
        render_app(&app, 60, 20);
        app.messages
            .push(DisplayMessage::new(MessageRole::Assistant, "extra content"));
        render_app(&app, 60, 20);
        assert!(
            app.render_state.scroll_compensation.get() > 0,
            "should have accumulated compensation"
        );

        // scroll to bottom
        app.scroll_offset = 0;
        render_app(&app, 60, 20);
        assert_eq!(
            app.render_state.scroll_compensation.get(),
            0,
            "compensation should reset when at bottom"
        );
    }

    #[test]
    fn start_streaming_preserves_scroll_offset() {
        // start_streaming should not yank the user to bottom
        // (push_user_message already does that when the user sends)
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.scroll_offset = 20;
        app.start_streaming();
        assert_eq!(
            app.scroll_offset, 20,
            "start_streaming should not reset scroll_offset"
        );
    }

    #[test]
    fn height_cache_converges_after_render() {
        // bug: height_cache stores an estimate from count_estimated_lines
        // (which counts raw markdown chars). once cached, it never rechecks
        // the indented_cache even after the message has been rendered with
        // exact line counts. the stale estimate persists across frames
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        // markdown-heavy content where raw char count overestimates lines.
        // **bold** markers add 4 invisible chars per emphasis
        let content = "**this is some very bold text that will definitely wrap at narrow widths** and **here is even more bold text for wrapping**";
        app.messages
            .push(DisplayMessage::new(MessageRole::Assistant, content));

        // first render at a narrow width to populate caches
        render_app(&app, 30, 20);

        // the height_cache should now have the actual rendered height,
        // not the overestimate from raw char counting
        let cache = app.render_state.height_cache.borrow();
        let entry = cache[0].as_ref().expect("height should be cached");
        let cached_height = entry.height;

        // compute actual height by checking how many indented lines were produced
        let indented = app.render_state.indented_cache.borrow();
        let (_, _, actual_lines) = indented[0].as_ref().expect("indented should be cached");
        let actual_content_lines = actual_lines.len();
        // total = separator (1) + content lines
        let actual_total = 1 + actual_content_lines;

        assert_eq!(
            cached_height, actual_total,
            "height_cache should converge to actual rendered height \
             (cached={cached_height}, actual={actual_total})"
        );
    }

    #[test]
    fn user_message_with_images_reserves_render_area() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut msg = DisplayMessage::new(MessageRole::User, "check this out");
        msg.images = vec![vec![0u8; 100]];
        app.messages.push(msg);
        let buf = render_app(&app, 60, 30);
        let content = buffer_to_string(&buf);
        assert!(content.contains("📷"));
        let areas = app.render_state.image_render_areas.borrow();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].msg_idx, 0);
        assert!(areas[0].area.height > 0);
    }
}
