//! tests for message_list

use super::*;
use crate::test_support::buffer_to_string;
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
fn compacting_renders_inline_busy_row_with_label() {
    // when /compact is in flight the message list grows a single
    // status row with the busy label, so the user sees what's
    // happening even when the status bar is hidden or full of other
    // text. without an active stream there would otherwise be no
    // on-chat indication of work
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.compacting = true;
    let buf = render_app(&app, 40, 10);
    let content = buffer_to_string(&buf);
    assert!(
        content.contains("compacting"),
        "expected 'compacting' label in chat, got:\n{content}"
    );
}

#[test]
fn inline_busy_rows_do_not_render_braille_spinners() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();

    let content = buffer_to_string(&render_app(&app, 40, 10));

    assert!(content.contains("working"));
    assert_no_braille_spinner(&content);
}

#[test]
fn compacting_busy_row_does_not_render_braille_spinner() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.compacting = true;

    let content = buffer_to_string(&render_app(&app, 40, 10));

    assert!(content.contains("compacting"));
    assert_no_braille_spinner(&content);
}

#[test]
fn image_overlay_cells_are_skipped_while_visible() {
    let area = Rect::new(1, 1, 2, 2);
    let mut buf = Buffer::empty(Rect::new(0, 0, 5, 5));

    mark_image_overlay_cells(area, &mut buf);

    for y in 1..3 {
        for x in 1..3 {
            assert_eq!(buf[(x, y)].diff_option, CellDiffOption::Skip);
        }
    }
}

fn assert_no_braille_spinner(content: &str) {
    for symbol in ["⠷", "⠯", "⠟", "⠻", "⠽", "⠾"] {
        assert!(
            !content.contains(symbol),
            "inline busy rows should use only the gradient label, got spinner symbol {symbol:?} in:\n{content}"
        );
    }
}

#[test]
fn compacting_row_disappears_once_flag_cleared() {
    // sanity: clearing the flag must remove the row so post-
    // compaction frames don't leave a ghost label in scrollback
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.compacting = true;
    let buf = render_app(&app, 40, 10);
    assert!(buffer_to_string(&buf).contains("compacting"));
    app.compacting = false;
    let buf = render_app(&app, 40, 10);
    assert!(!buffer_to_string(&buf).contains("compacting"));
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
fn long_offscreen_scrollback_does_not_materialise_placeholder_lines() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..1000 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message number {i}"),
        ));
    }

    let buf = render_app(&app, 80, 20);
    let content = buffer_to_string(&buf);
    let materialised = last_materialised_line_count();

    assert!(content.contains("message number 999"));
    assert!(
        materialised <= 80,
        "render should only materialise the visible window plus margin, got {materialised} lines"
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
fn user_message_without_images_uses_rounded_box() {
    // user messages render inside a rounded-corner box so the attachment
    // row design can share borders cleanly with the message body
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("hi clob");
    let buf = render_app(&app, 30, 8);
    let content = buffer_to_string(&buf);
    assert!(content.contains('╭'), "missing rounded top-left corner");
    assert!(content.contains('╮'), "missing rounded top-right corner");
    assert!(content.contains('╰'), "missing rounded bottom-left corner");
    assert!(content.contains('╯'), "missing rounded bottom-right corner");
    assert!(content.contains("hi clob"));
}

#[test]
fn user_message_box_pads_wide_chars_by_cells() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("界".repeat(12));

    let buf = render_app(&app, 28, 8);
    let content = buffer_to_string(&buf);
    let top_y = find_buffer_row(&buf, |line| line.contains('╭'));
    let body_y = find_buffer_row(&buf, |line| line.contains('界'));

    assert_eq!(
        right_edge_cell(&buf, body_y),
        right_edge_cell(&buf, top_y),
        "body row should align with border by terminal cells:\n{content}"
    );
}

#[test]
fn user_message_with_image_renders_attachment_row_above() {
    // attachments sit above the message box and share the message box's
    // top border via left/right tees. the attach row itself is a bordered
    // strip with square top corners and vertical dividers between cells
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message_with_images("look here", vec![vec![0u8; 100]]);
    let buf = render_app(&app, 40, 20);
    let content = buffer_to_string(&buf);

    assert!(content.contains('┌'), "missing attach top-left corner");
    assert!(content.contains('┐'), "missing attach top-right corner");
    // shared-divider row between attach and msg box: left-tee ├ and right-tee ┤
    assert!(
        content.contains('├'),
        "missing left-tee on attach/msg divider"
    );
    assert!(
        content.contains('┤'),
        "missing right-tee on attach/msg divider"
    );
    // msg body still closes with rounded bottom corners
    assert!(content.contains('╰'));
    assert!(content.contains('╯'));
    // and the message text is rendered
    assert!(content.contains("look here"));

    // image render area should land inside the attach cell (between the
    // top border and the divider row), so some area row must sit above
    // the msg box body
    let areas = app.render_state.image_render_areas.borrow();
    assert_eq!(areas.len(), 1, "image render area should be produced");
    // locate the divider row by scanning buffer for ├
    let divider_row = (0..buf.area.height)
        .find(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "├"))
        .expect("divider row with ├ present in buffer");
    assert!(
        areas[0].area.y < divider_row,
        "image render area (y={}) should sit above the divider row (y={divider_row})",
        areas[0].area.y
    );
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
fn edit_tool_renders_inline_diff_when_wide() {
    // edit tool output defaults to inline even on a wide terminal. the
    // old side-by-side threshold made small edits harder to scan by
    // splitting the changed lines across the whole tool panel width
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
    let any_row_with_both = content
        .lines()
        .any(|row| row.contains("println!(\"hello\")") && row.contains("println!(\"world\")"));
    assert!(
        !any_row_with_both,
        "wide edit diff should render inline, not side-by-side; got:\n{content}"
    );
    assert!(content.contains("hello"));
    assert!(content.contains("world"));
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
fn status_only_handles_streaming_thinking_label() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        thinking: Some("first i need to consider the question deeply".into()),
        ..DisplayMessage::new(MessageRole::Assistant, "the answer is 42")
    });
    let buf = render_app(&app, 60, 10);
    let content = buffer_to_string(&buf);
    assert!(!content.contains("💭"), "got:\n{content}");
    assert!(!content.contains("[ctrl+o]"), "got:\n{content}");
    assert!(content.contains("the answer is 42"), "got:\n{content}");
}

#[test]
fn thinking_with_leading_space_does_not_double_indent() {
    // Claude's thinking content typically starts each line with a single
    // leading space. indent_line prepends its own space so without
    // normalisation the body would render with two leading columns of
    // whitespace, looking over-indented relative to the busy header
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        thinking: Some(" I'm thinking about the problem\n I should reconsider".into()),
        thinking_expanded: true,
        ..DisplayMessage::new(MessageRole::Assistant, "answer")
    });
    let buf = render_app(&app, 60, 10);
    let content = buffer_to_string(&buf);
    for line in content.lines() {
        if let Some(col) = line.find("I'm thinking") {
            assert_eq!(
                col, 1,
                "expected 1-col indent for thinking body, got col {col} in line {line:?}"
            );
            return;
        }
    }
    panic!("thinking text not found in rendered buffer:\n{content}");
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
fn image_render_area_respects_bottom_anchor_padding() {
    // short content in a tall area gets bottom-anchored via top padding.
    // the image render y must include that padding so the overlay lands
    // next to its label instead of at the top of the area
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message_with_images("hi", vec![vec![0u8; 100]]);

    let _ = render_app(&app, 60, 40);
    let areas = app.render_state.image_render_areas.borrow();
    assert_eq!(areas.len(), 1);
    // user msg + label + image body is well under 40 rows, so padding
    // pushes the image past the first 15 rows of the area
    assert!(
        areas[0].area.y > 15,
        "image render y should account for bottom-anchor padding, got y={}",
        areas[0].area.y
    );
}

#[test]
fn image_render_areas_never_pin_to_top_when_scrolled_past() {
    // saturating_sub(scroll) used to clamp off-screen images to y=0
    // and stack them at the top of the viewport at full height. after
    // the fix, images that would need a negative screen-y are hidden
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for _ in 0..5 {
        app.push_user_message_with_images("hi", vec![vec![0u8; 100]]);
    }
    // bottom-anchored view of the newest content; older images are
    // scrolled off the top
    app.scroll_offset = 0;
    let _ = render_app(&app, 60, 30);

    let areas = app.render_state.image_render_areas.borrow();
    // no two render areas may share the same y: the buggy stacking
    // always placed every off-screen image at y=0
    let mut ys: Vec<u16> = areas.iter().map(|a| a.area.y).collect();
    ys.sort();
    let before = ys.len();
    ys.dedup();
    assert_eq!(
        ys.len(),
        before,
        "images are stacked on the same row (pin-to-top bug): {areas:?}"
    );
}

#[test]
fn image_render_area_hidden_when_partially_above_viewport() {
    // hide-on-partial policy: an image straddling the top edge of the
    // viewport is not rendered (ratatui_image would squish rather than
    // crop, and partial rendering needs upstream support)
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for _ in 0..5 {
        app.push_user_message_with_images("hi", vec![vec![0u8; 100]]);
    }
    app.scroll_offset = 10;
    let _ = render_app(&app, 60, 30);

    let areas = app.render_state.image_render_areas.borrow();
    for a in areas.iter() {
        assert_eq!(
            a.area.height, ATTACH_INNER_H,
            "render area has cropped height ({}) instead of full cell height \
             ({ATTACH_INNER_H}); partial images should be hidden: {a:?}",
            a.area.height
        );
    }
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

// grid layout for batched tools
#[test]
fn grid_dimensions_single_row_when_wide() {
    assert_eq!(grid_dimensions(4, 200, 30), (1, 4));
    assert_eq!(grid_dimensions(2, 80, 30), (1, 2));
}

#[test]
fn grid_dimensions_forms_grid_on_mid_width() {
    // 4 tools at width 80, min 30 → 80/30 = 2 cols, so 2x2
    assert_eq!(grid_dimensions(4, 80, 30), (2, 2));
    // 6 tools at width 100, min 30 → 100/30 = 3 cols, so 2x3
    assert_eq!(grid_dimensions(6, 100, 30), (2, 3));
}

#[test]
fn grid_dimensions_single_column_when_narrow() {
    assert_eq!(grid_dimensions(4, 30, 30), (4, 1));
    assert_eq!(grid_dimensions(3, 20, 30), (3, 1));
}

#[test]
fn four_batched_tools_render_as_2x2_grid_on_mid_width() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let make = |name: &str| crate::app::DisplayToolCall {
        name: "read".into(),
        summary: name.into(),
        status: ToolCallStatus::Done,
        output_preview: None,
        image_data: None,
        batch: 1,
    };
    app.messages.push(DisplayMessage {
        tool_calls: vec![make("a.rs"), make("b.rs"), make("c.rs"), make("d.rs")],
        ..DisplayMessage::new(MessageRole::Assistant, "reading")
    });
    // 80 wide: 80/30 = 2 cols → 2x2 grid, a+b on row 1, c+d on row 2
    let buf = render_app(&app, 80, 16);
    let content = buffer_to_string(&buf);
    // each row should have a ┬ junction (two panels joined)
    let junction_rows = content.lines().filter(|l| l.contains("┬")).count();
    assert_eq!(
        junction_rows, 2,
        "expected two grid rows with ┬ junctions, got:\n{content}"
    );
    // all four summaries visible
    for name in &["a.rs", "b.rs", "c.rs", "d.rs"] {
        assert!(content.contains(name), "missing {name} in:\n{content}");
    }
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
fn side_by_side_boxes_pad_wide_chars_by_cells() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let output = "界".repeat(18);
    app.messages.push(DisplayMessage {
        tool_calls: vec![
            crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "first".into(),
                status: ToolCallStatus::Done,
                output_preview: Some(output.clone()),
                image_data: None,
                batch: 1,
            },
            crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "second".into(),
                status: ToolCallStatus::Done,
                output_preview: Some("ok".into()),
                image_data: None,
                batch: 1,
            },
        ],
        ..DisplayMessage::new(MessageRole::Assistant, "running")
    });

    let buf = render_app(&app, 80, 16);
    let content = buffer_to_string(&buf);
    let top_y = find_buffer_row(&buf, |line| line.trim_start().starts_with('┌'));
    let body_y = find_buffer_row(&buf, |line| line.contains('界'));

    assert_eq!(
        right_edge_cell(&buf, body_y),
        right_edge_cell(&buf, top_y),
        "side-by-side body row should align with border by terminal cells:\n{content}"
    );
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
    // block 0 is the "here:" paragraph; block 1 is the bash fence body
    app.navigation.selected_block = Some(1);

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
fn wrap_text_uses_terminal_cell_width_for_wide_chars() {
    assert_eq!(wrap_text("界界界", 4), vec!["界界", "界"]);
}

#[test]
fn count_estimated_lines_uses_terminal_cell_width_for_wide_chars() {
    assert_eq!(count_estimated_lines("界界界", 4), 2);
}

#[test]
fn tool_box_wraps_long_summary() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let long_path = "/home/user/dev/mush/crates/mush-tui/src/widgets/message_list.rs";
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

#[test]
fn short_content_tool_box_sizes_to_content_not_available_width() {
    // a read tool with a short path on a wide terminal should render a
    // narrow box, not a full-width one. the border runs should end well
    // before the right edge
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "read".into(),
            summary: "a.rs".into(),
            status: ToolCallStatus::Done,
            output_preview: None,
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });
    let width = 120u16;
    let buf = render_app(&app, width, 8);
    let content = buffer_to_string(&buf);
    // find a line with the top border and confirm it doesn't stretch
    // across the whole terminal
    let top_line = content
        .lines()
        .find(|l| l.contains("┌"))
        .expect("top border row present");
    let box_width = top_line.trim_end().chars().count();
    assert!(
        box_width < width as usize,
        "expected content-sized box, got width {box_width} on {width}-col terminal"
    );
}

#[test]
fn long_content_tool_box_clamps_to_available_width() {
    // long summary that exceeds the terminal width should clamp to the
    // available width (same behaviour as before content-sizing)
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let long_path = "a".repeat(200);
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "read".into(),
            summary: long_path,
            status: ToolCallStatus::Done,
            output_preview: None,
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });
    let buf = render_app(&app, 40, 12);
    let content = buffer_to_string(&buf);
    let top_line = content
        .lines()
        .find(|l| l.contains("┌"))
        .expect("top border row present");
    // box spans most of the 40-col terminal (accounting for BOX_INDENT = 1)
    let box_width = top_line.trim_end().chars().count();
    assert!(
        box_width >= 38,
        "expected clamped-wide box, got {box_width}"
    );
}

#[test]
fn completed_tool_box_sizes_output_with_halfwidth_dakuten_by_cells() {
    let output = "abcｶﾞ".repeat(4);
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "noop".into(),
            summary: "ok".into(),
            status: ToolCallStatus::Done,
            output_preview: Some(output.clone()),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });

    let content = buffer_to_string(&render_app(&app, 80, 12));

    assert!(
        content.lines().any(|row| row.matches("ｶﾞ").count() == 4),
        "halfwidth dakuten output should fit on one row, got:\n{content}"
    );
}

#[test]
fn read_tool_box_pads_halfwidth_dakuten_by_cells() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "read".into(),
            summary: "src/lib.rs".into(),
            status: ToolCallStatus::Done,
            output_preview: Some(format!("L1: {}", "abcｶﾞ".repeat(4))),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });

    let buf = render_app(&app, 80, 12);
    let content = buffer_to_string(&buf);
    let top_y = find_buffer_row(&buf, |line| line.trim_start().starts_with('┌'));
    let body_y = find_buffer_row(&buf, |line| line.contains("abcｶﾞ"));

    assert_eq!(
        right_edge_cell(&buf, body_y),
        right_edge_cell(&buf, top_y),
        "read tool body row should align with border by terminal cells:\n{content}"
    );
}

// read tool output syntax highlighting
#[test]
fn parse_read_line_extracts_number_and_content() {
    assert_eq!(parse_read_line("L1: fn main() {"), Some((1, "fn main() {")));
    assert_eq!(
        parse_read_line("L123: let x = 1;"),
        Some((123, "let x = 1;"))
    );
}

#[test]
fn parse_read_line_returns_none_for_non_matching() {
    assert_eq!(parse_read_line("not a line"), None);
    assert_eq!(parse_read_line("L: missing number"), None);
    assert_eq!(parse_read_line("Label: not a line number"), None);
}

#[test]
fn parse_read_line_handles_empty_content() {
    // empty line in source still has the prefix but no content
    assert_eq!(parse_read_line("L5: "), Some((5, "")));
}

#[test]
fn read_line_number_width_is_widest_in_block() {
    let text = "L1: a\nL12: b\nL123: c";
    assert_eq!(read_line_number_width(text), 3);
}

#[test]
fn read_line_number_width_ignores_non_matching() {
    assert_eq!(read_line_number_width("L1: a\nnot a line"), 1);
    assert_eq!(read_line_number_width("nothing here"), 0);
}

#[test]
fn common_leading_space_returns_min_across_non_empty_lines() {
    assert_eq!(common_leading_space(" a\n  b\n c"), 1);
    assert_eq!(common_leading_space("  a\n  b"), 2);
    assert_eq!(common_leading_space("a\n  b"), 0);
    // blank lines ignored
    assert_eq!(common_leading_space(" a\n\n  b"), 1);
    assert_eq!(common_leading_space(""), 0);
}

#[test]
fn drop_leading_spaces_preserves_relative_indent() {
    assert_eq!(drop_leading_spaces(" hello", 1), "hello");
    assert_eq!(drop_leading_spaces("  nested", 1), " nested");
    assert_eq!(drop_leading_spaces("flush left", 1), "flush left");
    // fewer spaces than count removes only what's there
    assert_eq!(drop_leading_spaces(" x", 3), "x");
    // no-op on count=0
    assert_eq!(drop_leading_spaces("  body", 0), "  body");
}

#[test]
fn read_min_content_indent_computes_min_leading_spaces() {
    // min of 4/6/4 leading spaces across content is 4
    let text = "L1:     foo\nL2:       bar\nL3:     baz";
    assert_eq!(read_min_content_indent(text), 4);
}

#[test]
fn read_min_content_indent_ignores_blank_content_lines() {
    // blank content lines don't bias the min toward zero
    let text = "L1:     foo\nL2: \nL3:     bar";
    assert_eq!(read_min_content_indent(text), 4);
}

#[test]
fn read_min_content_indent_returns_zero_when_any_line_flush_left() {
    let text = "L1: foo\nL2:     bar";
    assert_eq!(read_min_content_indent(text), 0);
}

#[test]
fn read_min_content_indent_ignores_non_matching_lines() {
    let text = "L1:     foo\nplain narrative\nL2:       bar";
    assert_eq!(read_min_content_indent(text), 4);
}

#[test]
fn read_tool_box_trims_shared_leading_indent() {
    // regression: deeply nested code shown via the read tool used to
    // render with the raw source indent, pushing content halfway across
    // the preview box. the renderer must strip the common leading
    // whitespace so the smallest-indent row sits against the gutter
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "read".into(),
            summary: "src/main.rs".into(),
            status: ToolCallStatus::Done,
            output_preview: Some(
                "L1:         fn main() {\n\
                 L2:             println!(\"hi\");\n\
                 L3:         }"
                    .into(),
            ),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });
    let buf = render_app(&app, 120, 12);
    let content = buffer_to_string(&buf);

    let fn_line = content
        .lines()
        .find(|l| l.contains("fn main"))
        .expect("fn main row present");
    // work in char units: `│` is multibyte so byte positions mislead
    let pipe_char = fn_line.chars().position(|c| c == '│').unwrap();
    let fn_byte = fn_line.find("fn main").unwrap();
    let fn_char = fn_line[..fn_byte].chars().count();
    let gap = fn_char - pipe_char;
    // layout: │ + space + "1" + 2-space sep = 5 chars, fn should sit at +5
    assert!(
        gap <= 6,
        "fn main should sit against the gutter after trimming 8 leading \
         spaces, got char gap={gap} on line {fn_line:?}"
    );
}

#[test]
fn read_tool_box_renders_dim_line_numbers_with_highlighted_content() {
    // syntax-highlighted read output: line-number prefix dim, code
    // portion carries syntect-derived fg (multiple distinct colours
    // along the "fn main" row)
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "read".into(),
            summary: "src/main.rs".into(),
            status: ToolCallStatus::Done,
            output_preview: Some("L1: fn main() {\nL2:     println!(\"hi\");\nL3: }".into()),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });
    let buf = render_app(&app, 120, 12);

    // find the row containing "fn main" in the buffer
    let target_row = (0..buf.area.height)
        .find(|&y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("fn main")
        })
        .expect("no row containing 'fn main' in rendered output");

    // gather fg colours along that row for non-space cells inside the box
    let mut fgs = std::collections::HashSet::new();
    for x in 0..buf.area.width {
        let cell = &buf[(x, target_row)];
        let sym = cell.symbol();
        if sym.trim().is_empty() || matches!(sym, "│" | "┌" | "┐" | "└" | "┘" | "─") {
            continue;
        }
        fgs.insert(cell.fg);
    }
    assert!(
        fgs.len() >= 2,
        "expected multiple syntect fgs on highlighted read row, got {fgs:?}"
    );
}

#[test]
fn bash_tool_summary_is_syntax_highlighted() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "bash".into(),
            summary: "for file in *.rs; do echo $file; done".into(),
            status: ToolCallStatus::Done,
            output_preview: Some("done".into()),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });
    let buf = render_app(&app, 120, 12);

    let target_row = (0..buf.area.height)
        .find(|&y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("for file")
        })
        .expect("no row containing bash command in rendered output");

    let mut fgs = std::collections::HashSet::new();
    for x in 0..buf.area.width {
        let cell = &buf[(x, target_row)];
        let sym = cell.symbol();
        if sym.trim().is_empty() || matches!(sym, "│" | "┌" | "┐" | "└" | "┘" | "─") {
            continue;
        }
        fgs.insert(cell.fg);
    }
    assert!(
        fgs.len() >= 2,
        "expected multiple syntect fgs on highlighted bash row, got {fgs:?}"
    );
}

#[test]
fn bash_tool_output_is_syntax_highlighted_when_plain() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "bash".into(),
            summary: "script".into(),
            status: ToolCallStatus::Done,
            output_preview: Some("for file in *.rs; do echo $file; done".into()),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });
    let buf = render_app(&app, 120, 12);

    let target_row = (0..buf.area.height)
        .find(|&y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("for file")
        })
        .expect("no row containing bash output in rendered output");

    let mut fgs = std::collections::HashSet::new();
    for x in 0..buf.area.width {
        let cell = &buf[(x, target_row)];
        let sym = cell.symbol();
        if sym.trim().is_empty() || matches!(sym, "│" | "┌" | "┐" | "└" | "┘" | "─") {
            continue;
        }
        fgs.insert(cell.fg);
    }
    assert!(
        fgs.len() >= 2,
        "expected multiple syntect fgs on highlighted bash output row, got {fgs:?}"
    );
}

#[test]
fn stable_tool_boxes_reuse_highlighted_render() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "bash".into(),
            summary: "for file in *.rs; do echo $file; done".into(),
            status: ToolCallStatus::Done,
            output_preview: Some("for file in *.rs; do echo $file; done".into()),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "done")
    });

    crate::syntax::reset_highlight_line_call_count();
    render_app(&app, 80, 20);
    let first_calls = crate::syntax::highlight_line_call_count();
    assert!(
        first_calls > 0,
        "first render should highlight tool box text"
    );

    render_app(&app, 80, 20);

    assert_eq!(crate::syntax::highlight_line_call_count(), first_calls);
}

#[test]
fn cached_tool_boxes_preserve_image_order_by_batch() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![
            crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "first".into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: Some(vec![0u8; 100]),
                batch: 1,
            },
            crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "second".into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: None,
                batch: 2,
            },
        ],
        ..DisplayMessage::new(MessageRole::Assistant, "done")
    });

    let first = buffer_to_string(&render_app(&app, 80, 30));
    let second = buffer_to_string(&render_app(&app, 80, 30));

    assert_eq!(second, first);
    assert!(
        first.find("image").expect("image label should render")
            < first.find("second").expect("second tool should render"),
        "image label should stay with the first batch: {first}"
    );
}

#[test]
fn bash_tool_output_wraps_long_highlighted_lines() {
    let long_json = r#"{"duration_seconds":12.0,"exit_code":0,"status":"success","stderr":"Compiling mush-tui v0.1.0 (/home/rowan/dev/mush/crates/mush-tui) and then a lot more output that should wrap"}"#;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            name: "bash".into(),
            summary: "cargo nextest run".into(),
            status: ToolCallStatus::Done,
            output_preview: Some(long_json.into()),
            image_data: None,
            batch: 1,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });

    let buf = render_app(&app, 120, 12);
    let content = buffer_to_string(&buf);

    assert!(content.contains("duration_seconds"));
    assert!(content.contains("Compiling mush-tui"));
    assert!(
        !content.contains("lot more output that should wrap…"),
        "long highlighted bash output should wrap rather than use an inline ellipsis: {content}"
    );
}

#[test]
fn highlighted_tool_box_wraps_halfwidth_dakuten_by_cells() {
    let line = "abcｶﾞ".repeat(4);
    let rows = wrap_styled_spans(vec![Span::raw(line)], 15, Style::default());
    let rendered: Vec<String> = rows
        .iter()
        .map(|row| row.iter().map(|span| span.content.as_ref()).collect())
        .collect();

    assert_eq!(
        rendered,
        vec!["abcｶﾞabcｶﾞabcｶﾞ", "abcｶﾞ"],
        "halfwidth dakuten text should wrap by terminal cells"
    );
}

#[test]
fn truncate_text_reveals_by_chars_after_halfwidth_dakuten() {
    let text = Text::from(Line::raw("ｶﾞx"));

    let truncated = truncate_text(text, 2);

    assert_eq!(plain_text(&truncated), "ｶﾞ");
}

#[test]
fn truncate_text_reveals_by_chars_across_span_boundary_after_wide_char() {
    let text = Text::from(Line::from(vec![Span::raw("界"), Span::raw("ab")]));

    let truncated = truncate_text(text, 2);

    assert_eq!(plain_text(&truncated), "界a");
}

fn find_buffer_row(buf: &Buffer, predicate: impl Fn(&str) -> bool) -> u16 {
    (0..buf.area.height)
        .find(|&y| predicate(&buffer_row(buf, y)))
        .expect("buffer row present")
}

fn right_edge_cell(buf: &Buffer, y: u16) -> u16 {
    (0..buf.area.width)
        .rev()
        .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
        .expect("row has non-empty cell")
}

fn buffer_row(buf: &Buffer, y: u16) -> String {
    let mut row = String::new();
    for x in 0..buf.area.width {
        row.push_str(buf[(x, y)].symbol());
    }
    row
}

fn plain_text(text: &Text<'_>) -> String {
    let mut out = String::new();
    for line in &text.lines {
        for span in &line.spans {
            out.push_str(&span.content);
        }
    }
    out
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
fn indent_line_breaks_at_word_boundary_when_possible() {
    // "hello world foo" with content width 8 should break after "hello"
    // (at the space) rather than mid-word as "hello wo\nrld foo"
    let line = Line::raw("hello world foo");
    let wrapped = indent_line(line, 8);
    // inspect the produced lines' text content (ignore the leading space indent)
    let texts: Vec<String> = wrapped
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    // each wrapped line includes the leading " " indent span
    // strip it for assertion clarity
    let bodies: Vec<&str> = texts.iter().map(|s| s.trim_start_matches(' ')).collect();
    assert!(
        bodies.iter().all(|b| !b.ends_with(' ')),
        "no row should end with a trailing space: {bodies:?}"
    );
    assert!(
        bodies.iter().all(|b| !b.starts_with(' ')),
        "no row body should start with a space (ghost indent): {bodies:?}"
    );
    assert!(
        bodies.iter().any(|b| b == &"hello"),
        "expected a row containing exactly \"hello\" (clean word break), got {bodies:?}"
    );
}

#[test]
fn indent_line_falls_back_to_char_split_for_long_words() {
    // a word longer than content width must still wrap char-wise
    // (word-aware fallback); don't lose any characters
    let line = Line::raw("abcdefghijklmnop");
    let wrapped = indent_line(line, 5);
    let bodies: Vec<String> = wrapped
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_start_matches(' ')
                .to_string()
        })
        .collect();
    let joined = bodies.join("");
    assert_eq!(
        joined, "abcdefghijklmnop",
        "all chars preserved across wrap: {bodies:?}"
    );
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
fn direct_render_caches_code_block_highlight() {
    // mirrors the render_loop example: render the widget directly,
    // repeatedly, with a stable code-block conversation. highlighting
    // must happen once, not per frame.
    crate::markdown::reset_highlight_code_block_call_count();
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for _ in 0..4 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "## summary\n\n```rust\nfn main() {\n    let x = 42;\n}\n```\n",
        ));
    }
    let area = Rect::new(0, 0, 100, 40);
    let mut buf = Buffer::empty(area);
    crate::markdown::reset_render_call_count();
    MessageList::new(&app).render(area, &mut buf);
    let first = crate::markdown::highlight_code_block_call_count();
    let first_parse = crate::markdown::render_call_count();
    for _ in 0..5 {
        MessageList::new(&app).render(area, &mut buf);
    }
    let after = crate::markdown::highlight_code_block_call_count();
    let after_parse = crate::markdown::render_call_count();
    assert_eq!(
        first, after,
        "re-highlighted across renders: {first} -> {after}"
    );
    assert_eq!(
        first_parse, after_parse,
        "re-parsed markdown across renders: {first_parse} -> {after_parse}"
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
fn scroll_down_after_content_growth_steps_by_scroll_lines() {
    // repro: sit a few lines above the bottom, let a long message land
    // (the viewport stays pinned via compensation), then press j once.
    // scroll_offset alone would hit 0 and snap the viewport to the
    // real bottom, skipping everything that arrived in between
    const STEP: u16 = 3;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.scroll_lines = STEP;
    app.navigation.scroll_unit = crate::app::ScrollUnit::Message;
    app.interaction.mode = crate::app::AppMode::Scroll;

    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i}"),
        ));
    }
    render_app(&app, 60, 20);

    app.scroll_offset = STEP;
    render_app(&app, 60, 20);
    let pinned = app.render_state.render_scroll.get();
    assert!(pinned > 0, "should be scrolled up");

    let long_body = (0..50)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, long_body));
    render_app(&app, 60, 20);
    assert_eq!(
        pinned,
        app.render_state.render_scroll.get(),
        "viewport stays pinned while content grows"
    );
    let growth = app.render_state.scroll_compensation.get();
    assert!(growth > STEP as isize, "content should have grown a lot");

    let j = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('j'),
        crossterm::event::KeyModifiers::NONE,
    );
    crate::input::handle_key(&mut app, j);
    render_app(&app, 60, 20);

    assert_eq!(
        app.render_state.render_scroll.get(),
        pinned + STEP,
        "one j should move down exactly scroll_lines, not jump to the bottom"
    );
    // the offset started STEP lines above the bottom and `growth` lines
    // landed below it, so stepping STEP down leaves `growth` to go
    assert_eq!(
        app.scroll_offset as isize, growth,
        "scroll_offset should absorb the growth so it stays true \
         lines-from-bottom"
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

/// repro for the "j doesn't go down after resize during streaming"
/// bug: after a resize while scrolled up, the next render's stale
/// `prev_content_lines` (computed under the OLD wrap width) becomes
/// the baseline. if the new width produces a smaller line count,
/// `compensation = total_new - baseline_old` is negative and big
/// enough to clamp `effective_offset` to 0 - locking the viewport
/// at max_scroll regardless of how many `j` presses arrive.
///
/// fix invariant: after a resize, pressing `j` from the same
/// scroll_offset must produce a strictly larger `render_scroll`
/// (= viewport moved down) than not pressing it.
#[test]
fn j_still_scrolls_down_after_resize_during_scroll() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    // long message bodies so wrap width genuinely changes total
    // line count: a 200-char paragraph wraps to ~10 lines at width
    // 20 vs ~3 lines at width 80
    for i in 0..40 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} ").repeat(20),
        ));
    }

    // scroll up at the narrow width so prev_content_lines captures
    // a large total (lots of wrap)
    app.scroll_offset = 30;
    render_app(&app, 30, 20);
    // settle: a second render so the baseline moves out of the
    // first-frame capture branch
    render_app(&app, 30, 20);

    // simulate the resize handler: notify_resize fires before the
    // first post-resize render
    app.notify_resize();

    // first post-resize render at a much wider width
    render_app(&app, 120, 20);
    let render_scroll_after_resize = app.render_state.render_scroll.get();

    // press j: scroll_offset decreases
    let prev_offset = app.scroll_offset;
    app.scroll_down(app.scroll_lines);
    assert!(
        app.scroll_offset < prev_offset,
        "j should reduce scroll_offset; if equal, scroll_lines is 0 - check test setup"
    );

    // re-render: viewport must move DOWN, i.e. render_scroll
    // (= max_scroll - effective_offset) must increase
    render_app(&app, 120, 20);
    let render_scroll_after_j = app.render_state.render_scroll.get();
    assert!(
        render_scroll_after_j > render_scroll_after_resize,
        "j must move the viewport down after a resize, but render_scroll \
         went from {render_scroll_after_resize} to {render_scroll_after_j}"
    );
}

#[test]
fn scroll_position_stable_when_content_fluctuates() {
    // bug: while scrolled up during streaming, the message log "shakes"
    // by ±1 line. root cause: compensation uses `saturating_sub` so
    // only positive deltas update it. when content height oscillates
    // (word-wrap reflowing as tokens arrive, trailing estimate drifting
    // between frames) shrinks are absorbed silently, so effective
    // offset is too high once content grows back, drifting the view
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..20 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message number {i} with some stable content"),
        ));
    }
    // give the last message real multi-line content so shrink/grow
    // actually changes the rendered line count
    let long_multiline = "line a is here\nline b is here\nline c is here\nline d is here";
    app.messages.last_mut().unwrap().content = long_multiline.into();
    app.scroll_offset = 15;

    render_app(&app, 60, 20);
    let scroll_baseline = app.render_state.render_scroll.get();
    assert!(scroll_baseline > 0, "precondition: viewport scrolled up");

    // shrink last message by collapsing to a single line
    app.messages.last_mut().unwrap().content = "just one line".into();
    render_app(&app, 60, 20);
    let scroll_after_shrink = app.render_state.render_scroll.get();
    assert_eq!(
        scroll_baseline, scroll_after_shrink,
        "viewport should stay pinned to the same content line when \
         content shrinks while scrolled (baseline={scroll_baseline}, \
         after_shrink={scroll_after_shrink})"
    );

    // grow back to original height
    app.messages.last_mut().unwrap().content = long_multiline.into();
    render_app(&app, 60, 20);
    let scroll_after_regrow = app.render_state.render_scroll.get();
    assert_eq!(
        scroll_baseline, scroll_after_regrow,
        "viewport should return to baseline after grow restores content \
         (baseline={scroll_baseline}, after_regrow={scroll_after_regrow})"
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
fn scrolled_up_during_streaming_shows_no_gap_in_viewport() {
    // bug: while streaming, `estimated_total` for `should_render`
    // only counts messages, but the final `scroll` uses total lines
    // (messages + streaming + queued). when streaming content is
    // taller than the margin, messages actually visible in the
    // viewport fall outside the should_render window and render as
    // blank placeholder rows. the user sees gaps mid-scrollback.
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..40 {
        let role = if i % 2 == 0 {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        app.messages.push(DisplayMessage::new(
            role,
            format!("MSG-{i:03}-marker-{}", "x".repeat(i % 5)),
        ));
    }

    // warm caches so heights are known
    render_app(&app, 60, 20);

    // start streaming with long text (many lines) that exceeds the
    // viewport margin. this is the scenario that triggers the gap
    app.start_streaming();
    let long_stream: String = (0..80).map(|i| format!("stream line {i}\n")).collect();
    app.push_text_delta(&long_stream);
    // drain typewriter so the full text is visible
    for _ in 0..80 {
        app.tick();
    }

    // scroll up far enough that some messages are in the viewport
    // despite the long streaming content appended below. with vh=20
    // and streaming_N=80, effective_offset must exceed streaming_N
    // for messages to enter the visible window
    app.scroll_offset = 90;

    let buf = render_app(&app, 60, 20);
    let content = buffer_to_string(&buf);

    // at least one message marker should appear in the viewport
    let any_msg_visible = (0..40).any(|i| content.contains(&format!("MSG-{i:03}-marker")));
    assert!(
        any_msg_visible,
        "viewport should contain at least one message when scrolled up: \n{content}"
    );

    // count consecutive blank rows. a long run of blanks in the
    // middle of the viewport indicates placeholder lines where
    // real message content should have been rendered
    let rows: Vec<&str> = content.lines().collect();
    let mut consecutive_blanks = 0;
    let mut max_consecutive_blanks = 0;
    for row in &rows {
        if row.trim().is_empty() {
            consecutive_blanks += 1;
            max_consecutive_blanks = max_consecutive_blanks.max(consecutive_blanks);
        } else {
            consecutive_blanks = 0;
        }
    }
    assert!(
        max_consecutive_blanks < 5,
        "found {max_consecutive_blanks} consecutive blank rows in viewport: \n{content}"
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
fn scrolling_continues_to_work_after_terminal_resize_grows_viewport() {
    // regression: when the user is scrolled up and the terminal is
    // resized so the message-list area grows by more than the
    // current scroll_offset, the existing baseline_vis pin makes
    // compensation negative. effective_offset clamps to 0 and the
    // first N presses of k (or j) appear to do nothing because
    // scroll_from_top stays at max_scroll. only after enough k
    // presses to overcome the negative compensation does the view
    // start moving. that's the "scrolling stopped working until you
    // resize back" symptom rowan reported. the fix is to reset the
    // scroll baseline on a real resize so the math doesn't carry
    // stale baseline_vis into the new dimensions
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} body content"),
        ));
    }
    // initial render at one size, scroll up a little
    render_app(&app, 60, 20);
    app.scroll_offset = 3;
    render_app(&app, 60, 20);
    let scroll_before = app.render_state.render_scroll.get();
    assert!(scroll_before > 0, "precondition: scrolled up");

    // simulate a real resize that grows the viewport by more than
    // the current scroll_offset
    app.notify_resize();
    render_app(&app, 60, 28);
    let scroll_after_resize = app.render_state.render_scroll.get();

    // press k once: scroll should move by scroll_lines, not be
    // swallowed by stale baseline_vis compensation
    app.scroll_up(app.scroll_lines);
    render_app(&app, 60, 28);
    let scroll_after_k = app.render_state.render_scroll.get();

    assert_ne!(
        scroll_after_resize, scroll_after_k,
        "single k press after resize must change scroll position \
         (post-resize scroll={scroll_after_resize}, after k={scroll_after_k}). \
         when this fails the user has to press k repeatedly with no \
         visible feedback before scrolling resumes"
    );
    assert!(
        scroll_after_k < scroll_after_resize,
        "k should scroll up (decrease scroll_from_top): \
         {scroll_after_resize} -> {scroll_after_k}"
    );
}

#[test]
fn scrolling_without_resize_notification_is_swallowed_by_stale_baseline() {
    // when the message-list area grows (e.g. because a tool panel
    // disappeared, or the status bar unwrapped) without going through
    // notify_resize, the cached baseline_vis is now smaller than the
    // current vis. compensation goes negative and absorbs the user's
    // first N k/j keypresses. previously the only callsite for
    // notify_resize was the terminal Event::Resize handler; the same
    // family of stale-baseline bugs also fires on tool-panel resize.
    // the fix re-pins the baseline at render time when current_vis
    // grows past stored_vis, since a grown viewport means the pin's
    // implicit "viewport shrank from baseline" assumption is no
    // longer true
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} body content"),
        ));
    }
    render_app(&app, 60, 20);
    app.scroll_offset = 3;
    render_app(&app, 60, 20);

    // simulate the message-list area growing by 8 lines (tool panel
    // disappeared, or similar) without notify_resize. with the fix,
    // baseline gets re-pinned and the next keypress moves the view
    render_app(&app, 60, 28);
    let scroll_after_grow = app.render_state.render_scroll.get();

    app.scroll_up(app.scroll_lines);
    render_app(&app, 60, 28);
    let scroll_after_k = app.render_state.render_scroll.get();

    assert_ne!(
        scroll_after_grow, scroll_after_k,
        "first k press after vis grows must move the viewport \
         (post-grow={scroll_after_grow}, after_k={scroll_after_k}). \
         when this fails the user has to press k repeatedly with no \
         visible feedback before scrolling resumes - the same family \
         of bug as the resize fix but triggered by tool-panel changes"
    );
    assert!(
        scroll_after_k < scroll_after_grow,
        "k should scroll up (decrease scroll_from_top): \
         {scroll_after_grow} -> {scroll_after_k}"
    );
}

#[test]
fn j_key_responds_immediately_after_vis_grows() {
    // bug report: "k works to go up, but sometimes j doesn't work
    // to go down" in scroll mode. when vis grew without notify_resize
    // (tool panel disappeared, etc), compensation went negative and
    // the user was effectively stuck at scroll_offset 0 even though
    // the displayed view appeared scrolled up. j (saturating_sub)
    // was a no-op because effective_offset stayed clamped at 0,
    // while k (saturating_add) appeared "weak" but eventually
    // overcame the clamp.
    //
    // pick scroll_offset small enough that scroll_offset +
    // compensation < 0, so without the fix j would be a no-op
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} body content"),
        ));
    }
    render_app(&app, 60, 20);
    app.scroll_offset = 5;
    render_app(&app, 60, 20);
    let scroll_before_grow = app.render_state.render_scroll.get();

    // vis grows by 8 (tool panel disappeared, etc). without the fix
    // compensation = (20 - 28) = -8 and effective_offset clamps to
    // max(0, 5 - 8) = 0, so the view jumps to the bottom and the
    // next j becomes a no-op
    render_app(&app, 60, 28);
    let scroll_before_j = app.render_state.render_scroll.get();

    // press j: scroll_offset goes 5 → 2
    app.scroll_down(app.scroll_lines);
    render_app(&app, 60, 28);
    let scroll_after_j = app.render_state.render_scroll.get();

    assert_ne!(
        scroll_before_j, scroll_after_j,
        "j press after vis grows must move the viewport \
         (before_j={scroll_before_j}, after_j={scroll_after_j}). \
         when this assertion fails, j is silently no-op'ing because \
         effective_offset clamped to 0. baseline still respected \
         before_grow={scroll_before_grow}"
    );
    assert!(
        scroll_after_j > scroll_before_j,
        "j should scroll down (increase scroll_from_top): \
         {scroll_before_j} -> {scroll_after_j}"
    );
}

#[test]
fn scroll_stays_stable_when_viewport_height_jitters() {
    // regression: when the status bar wraps between 1 and 2 lines
    // (cache countdown text changing width, scroll-position indicator
    // appearing, etc), the message area's `vis` shrinks/grows by 1.
    // the existing baseline anchor only compensates for total-line
    // changes, not viewport-height changes, so the viewport jumps.
    // simulate this by rendering at alternating heights
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} stuff"),
        ));
    }
    render_app(&app, 60, 20);
    app.scroll_offset = 15;
    render_app(&app, 60, 20);
    let anchor = app.render_state.render_scroll.get();

    // simulate status bar growing 1 line (message area shrinks 1)
    render_app(&app, 60, 19);
    let after_shrink = app.render_state.render_scroll.get();

    // simulate status bar shrinking back (message area grows back)
    render_app(&app, 60, 20);
    let after_regrow = app.render_state.render_scroll.get();

    assert_eq!(
        anchor, after_shrink,
        "viewport must not shift when vis shrinks by 1 \
         (anchor={anchor}, after_shrink={after_shrink})"
    );
    assert_eq!(
        anchor, after_regrow,
        "viewport must return to baseline after vis grows back \
         (anchor={anchor}, after_regrow={after_regrow})"
    );
}

#[test]
fn scroll_stays_stable_while_streaming_typewriter_reveals_text() {
    // regression: while scrolled up during streaming, the viewport drifts
    // each frame because compensation is computed from *estimated* total
    // (count_estimated_lines on raw chars) but scroll uses *actual* total
    // (markdown-rendered lines). when the estimate over- or under-counts
    // (e.g. code fences, heading style rows, `**bold**` markers), the
    // effective_offset diverges from actual content growth and the view
    // creeps up/down by the estimate error.
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} with stable content here"),
        ));
    }
    // warm caches
    render_app(&app, 60, 20);

    // scroll up into history
    app.scroll_offset = 15;
    render_app(&app, 60, 20);
    let scroll_anchor = app.render_state.render_scroll.get();

    // stream markdown chunks where char-count estimate diverges from
    // the actual rendered line count (code fence, bold, heading).
    // drive typewriter between chunks the way real streaming does.
    app.start_streaming();
    let chunks = [
        "# heading\n",
        "here is some **bold** text\n",
        "```rust\n",
        "let x = 1;\n",
        "let y = 2;\n",
        "```\n",
    ];
    let mut scrolls = vec![scroll_anchor];
    for chunk in chunks {
        app.push_text_delta(chunk);
        for _ in 0..8 {
            app.tick();
            render_app(&app, 60, 20);
            scrolls.push(app.render_state.render_scroll.get());
        }
    }

    // viewport should stay pinned at its original content line with
    // zero drift: no ±1 shimmer while scrolled up and streaming.
    // any movement of `render_scroll` here is the bug.
    let min = *scrolls.iter().min().unwrap();
    let max = *scrolls.iter().max().unwrap();
    assert_eq!(
        min, max,
        "render_scroll drifted during streaming (anchor={scroll_anchor}, \
         min={min}, max={max}): {scrolls:?}"
    );
}

#[test]
fn scroll_stays_stable_when_streaming_thinking_then_text() {
    // regression: when a thinking block streams first and then the
    // main text starts, the streaming layout adds new lines (the
    // thinking section block + separator) which can shift the viewport
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} with content"),
        ));
    }
    render_app(&app, 60, 20);
    app.scroll_offset = 15;
    render_app(&app, 60, 20);
    let anchor = app.render_state.render_scroll.get();

    let mut scrolls = vec![anchor];
    app.start_streaming();
    // expand thinking so it's visible, not collapsed
    app.thinking_display = crate::app::ThinkingDisplay::Expanded;

    // stream thinking deltas
    for chunk in ["thinking ", "about ", "things\n", "and more\n"] {
        app.push_thinking_delta(chunk);
        for _ in 0..6 {
            app.tick();
            render_app(&app, 60, 20);
            scrolls.push(app.render_state.render_scroll.get());
        }
    }

    // now main text starts streaming
    for chunk in ["here ", "is the answer\n", "with details\n"] {
        app.push_text_delta(chunk);
        for _ in 0..6 {
            app.tick();
            render_app(&app, 60, 20);
            scrolls.push(app.render_state.render_scroll.get());
        }
    }

    let min = *scrolls.iter().min().unwrap();
    let max = *scrolls.iter().max().unwrap();
    assert_eq!(
        min, max,
        "viewport drifted during thinking->text transition \
         (anchor={anchor}, min={min}, max={max}): {scrolls:?}"
    );
}

#[test]
fn streaming_markdown_cache_is_keyed_by_width() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.text =
        "| Name | Notes |\n|------|-------|\n| Ada | this is a long table cell |".into();

    let _ = render_streaming_markdown_cached(&app, &app.stream.text, 60);
    assert_eq!(
        app.render_state
            .stream_markdown_cache
            .borrow()
            .as_ref()
            .map(|(width, _, _)| *width),
        Some(60)
    );

    let _ = render_streaming_markdown_cached(&app, &app.stream.text, 24);
    assert_eq!(
        app.render_state
            .stream_markdown_cache
            .borrow()
            .as_ref()
            .map(|(width, _, _)| *width),
        Some(24)
    );
}

#[test]
fn streaming_markdown_skips_syntax_highlighting() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.text = "```rust\nlet x = 42;\n```".into();

    crate::markdown::reset_highlight_code_block_call_count();

    let text = render_streaming_markdown_cached(&app, &app.stream.text, 60);

    assert!(
        text.lines
            .iter()
            .any(|line| line.to_string().contains("let x"))
    );
    assert_eq!(crate::markdown::highlight_code_block_call_count(), 0);
}

#[test]
fn rendering_outside_block_scroll_does_not_build_block_list() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.interaction.mode = crate::app::AppMode::Scroll;
    app.navigation.scroll_unit = crate::app::ScrollUnit::Message;
    for i in 0..100 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i}\n\n```rust\nlet x = {i};\n```"),
        ));
    }

    crate::app::reset_blocks_call_count();

    render_app(&app, 60, 20);

    assert_eq!(crate::app::blocks_call_count(), 0);
}

#[test]
fn scroll_stays_stable_when_streaming_starts_from_empty() {
    // regression: when streaming begins, the "working" busy line
    // appears, then disappears as the first text chunk arrives. the
    // working line is 1 line, the first chunk may also be 1 line, so
    // the transition should net zero. but if the line counts differ
    // by 1 between estimate and actual, the viewport shifts
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} body content"),
        ));
    }
    render_app(&app, 60, 20);
    app.scroll_offset = 15;
    render_app(&app, 60, 20);
    let anchor = app.render_state.render_scroll.get();

    let mut scrolls = vec![anchor];

    // streaming starts: render with empty stream (working label visible)
    app.start_streaming();
    for _ in 0..4 {
        app.tick();
        render_app(&app, 60, 20);
        scrolls.push(app.render_state.render_scroll.get());
    }

    // first text arrives
    app.push_text_delta("first ");
    for _ in 0..4 {
        app.tick();
        render_app(&app, 60, 20);
        scrolls.push(app.render_state.render_scroll.get());
    }

    let min = *scrolls.iter().min().unwrap();
    let max = *scrolls.iter().max().unwrap();
    assert_eq!(
        min, max,
        "viewport drifted at streaming start (anchor={anchor}, \
         min={min}, max={max}): {scrolls:?}"
    );
}

#[test]
fn scroll_stays_stable_across_busy_label_ticks_only() {
    // regression: each tick advances the busy label styling. the
    // label text stays on an existing line, so total line count
    // shouldn't change. verify the animation alone doesn't cause
    // viewport drift
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..30 {
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("message {i} stuff"),
        ));
    }
    render_app(&app, 60, 20);
    app.scroll_offset = 15;
    render_app(&app, 60, 20);
    let anchor = app.render_state.render_scroll.get();

    // streaming with stable content. drive ticks but no new deltas
    app.start_streaming();
    app.push_text_delta("stable text\nthat doesnt change\n");
    // let the typewriter fully reveal
    for _ in 0..200 {
        app.tick();
    }
    render_app(&app, 60, 20);

    let mut scrolls = vec![app.render_state.render_scroll.get()];
    for _ in 0..30 {
        app.tick();
        render_app(&app, 60, 20);
        scrolls.push(app.render_state.render_scroll.get());
    }

    let min = *scrolls.iter().min().unwrap();
    let max = *scrolls.iter().max().unwrap();
    assert_eq!(
        min, max,
        "busy-label-only ticks shouldn't move the viewport \
         (anchor={anchor}, min={min}, max={max})"
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
    // new attachment-box layout shows the title inside the top border
    assert!(
        content.contains("attachment"),
        "attachment title should appear in the top border"
    );
    let areas = app.render_state.image_render_areas.borrow();
    assert_eq!(areas.len(), 1);
    assert_eq!(areas[0].msg_idx, 0);
    assert!(areas[0].area.height > 0);
}
