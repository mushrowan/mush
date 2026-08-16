use super::*;
use crate::test_support::buffer_to_string;
use mush_ai::types::{Dollars, Usage};

fn all_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let style = MessageStyle {
        theme: &app.theme,
        show_usage: app.interaction.show_usage_lines,
    };
    let mut lines = Vec::new();
    for msg in app.messages.iter() {
        lines.extend(message_lines(msg, &style, width));
        lines.push(Line::raw(""));
    }
    lines.extend(streaming_lines(app, width));
    lines
}

fn render_app(app: &App, width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    write_lines_to_buf(&all_lines(app, width), &mut buf, area, 0);
    buf
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
    let content = buffer_to_string(&buf);
    assert!(content.trim().is_empty());
}

#[test]
fn compacting_renders_inline_busy_row_with_label() {
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
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.compacting = true;
    let buf = render_app(&app, 40, 10);
    assert!(buffer_to_string(&buf).contains("compacting"));
    app.compacting = false;
    let buf = render_app(&app, 40, 10);
    assert!(!buffer_to_string(&buf).contains("compacting"));
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
fn a_user_message_is_a_tinted_block_with_no_border() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("hi clob");
    let buf = render_app(&app, 30, 8);
    let content = buffer_to_string(&buf);

    assert!(content.contains("hi clob"));
    assert!(
        !content.contains(['╭', '╮', '╰', '╯', '│', '─']),
        "user messages should not be boxed:\n{content}"
    );

    let body = find_buffer_row(&buf, |line| line.contains("hi clob"));
    assert_eq!(buffer_row(&buf, body).find("hi clob"), Some(2));
    for y in [body - 1, body, body + 1] {
        assert_eq!(
            buf[(0, y)].bg,
            app.theme.user_msg_bg.bg.unwrap(),
            "row {y} should carry the user tint:\n{content}"
        );
    }
}

#[test]
fn a_user_message_pads_wide_chars_by_cells() {
    let theme = crate::theme::Theme::default();
    let mut msg = DisplayMessage::new(MessageRole::User, "界".repeat(12));
    msg.queued = false;

    for line in message_lines(&msg, &plain_style(&theme), 28) {
        assert_eq!(
            line_cell_width(&line),
            28,
            "every tinted row fills the width: {:?}",
            rendered_text(std::slice::from_ref(&line))
        );
    }
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
    for _ in 0..10 {
        app.tick();
    }
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
    assert!(content.contains("reuse 50%"));
    assert!(content.contains("write 16%"));
    assert!(content.contains("$0.0012"));
}

#[test]
fn usage_line_reuse_reflects_cache_bust() {
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
    assert!(
        content.contains("reuse 4%"),
        "during a cache bust reuse should reflect total input, got: {content}"
    );
}

#[test]
fn usage_line_hidden_by_default_shown_when_enabled() {
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

    app.interaction.show_usage_lines = true;
    let shown = buffer_to_string(&render_app(&app, 70, 10));
    assert!(
        shown.contains("320tok"),
        "usage line should render when toggled on, got: {shown}"
    );
}

#[test]
fn usage_line_deferred_while_tool_running() {
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

    let mut msg = app.messages.get_mut(0).expect("message");
    msg.tool_calls[0].status = ToolCallStatus::Done;
    msg.tool_calls[0].output_preview = Some("ok".into());
    drop(msg);
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

    let thinking_fg = app.theme.thinking.fg;
    let dim_fg = app.theme.dim.fg;
    assert_ne!(thinking_fg, dim_fg, "thinking and dim should differ");

    let thinking_cell = (0..buf.area.height)
        .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
        .find(|&(x, y)| {
            let cell = &buf[(x, y)];
            cell.symbol() == "d" && {
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
fn a_completed_tool_is_a_tinted_panel_with_a_status_header() {
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

    assert!(
        !content.contains(['┌', '└', '┐', '┘', '│']),
        "a tool call should not be boxed:\n{content}"
    );
    let header = find_buffer_row(&buf, |line| line.contains("bash"));
    assert_eq!(buffer_row(&buf, header).trim_end(), "  bash · done");
    assert!(content.contains("cargo test"), "missing summary");

    let tint = app.theme.tool_panel_bg.bg.unwrap();
    let body = find_buffer_row(&buf, |line| line.contains("cargo test"));
    assert_eq!(buffer_row(&buf, body).find("cargo test"), Some(2));
    for y in [header, body] {
        for x in [0, 2, buf.area.width - 1] {
            assert_eq!(
                buf[(x, y)].bg,
                tint,
                "cell {x},{y} should keep the panel tint:\n{content}"
            );
        }
    }
}

#[test]
fn batched_tools_stack_one_panel_per_call() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let tool = |name: &str| crate::app::DisplayToolCall {
        name: name.into(),
        summary: format!("{name} summary"),
        status: ToolCallStatus::Done,
        output_preview: None,
        image_data: None,
        batch: 1,
    };
    app.messages.push(DisplayMessage {
        tool_calls: vec![tool("read"), tool("grep"), tool("bash"), tool("edit")],
        ..DisplayMessage::new(MessageRole::Assistant, "looking")
    });
    let buf = render_app(&app, 100, 20);
    let content = buffer_to_string(&buf);

    let rows: Vec<u16> = ["read", "grep", "bash", "edit"]
        .iter()
        .map(|n| find_buffer_row(&buf, |line| line.contains(&format!("  {n} "))))
        .collect();
    let mut sorted = rows.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(rows, sorted, "panels should stack in order:\n{content}");
}

#[test]
fn a_failed_tool_says_error_in_red() {
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

    let header = find_buffer_row(&buf, |line| line.contains("bash"));
    assert_eq!(buffer_row(&buf, header).trim_end(), "  bash \u{b7} error");
    assert!(
        content.contains("E0063"),
        "missing error output:\n{content}"
    );

    let red = app.theme.tool_error.fg.unwrap();
    assert_eq!(buf[(2, header)].fg, red, "tool name should be red");
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
    let buf = render_app(&app, 40, 12);
    let content = buffer_to_string(&buf);
    assert!(
        content.contains("message_list.rs"),
        "path end should be visible after wrapping"
    );
    assert!(!content.contains("…"), "should wrap, not truncate");
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
    let body = find_buffer_row(&buf, |line| line.contains("abcｶﾞ"));

    assert_eq!(
        buf[(79, body)].bg,
        app.theme.tool_panel_bg.bg.unwrap(),
        "half-width dakuten should pad by cells to the panel edge:\n{content}"
    );
}

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
    assert_eq!(common_leading_space(" a\n\n  b"), 1);
    assert_eq!(common_leading_space(""), 0);
}

#[test]
fn drop_leading_spaces_preserves_relative_indent() {
    assert_eq!(drop_leading_spaces(" hello", 1), "hello");
    assert_eq!(drop_leading_spaces("  nested", 1), " nested");
    assert_eq!(drop_leading_spaces("flush left", 1), "flush left");
    assert_eq!(drop_leading_spaces(" x", 3), "x");
    assert_eq!(drop_leading_spaces("  body", 0), "  body");
}

#[test]
fn read_min_content_indent_computes_min_leading_spaces() {
    let text = "L1:     foo\nL2:       bar\nL3:     baz";
    assert_eq!(read_min_content_indent(text), 4);
}

#[test]
fn read_min_content_indent_ignores_blank_content_lines() {
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
    let gap = fn_line.chars().take_while(|c| *c == ' ').count();
    assert!(
        gap <= 6,
        "fn main should sit against the gutter after trimming 8 leading \
         spaces, got char gap={gap} on line {fn_line:?}"
    );
}

#[test]
fn read_tool_box_renders_dim_line_numbers_with_highlighted_content() {
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

    let target_row = (0..buf.area.height)
        .find(|&y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("fn main")
        })
        .expect("no row containing 'fn main' in rendered output");

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
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "this line is way too long to fit in twenty characters",
    ));
    let buf = render_app(&app, 20, 10);
    let text = buffer_to_string(&buf);
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
    let line = Line::raw("hello world foo");
    let wrapped = indent_line(line, 8);
    let texts: Vec<String> = wrapped
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
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

    app.tick();

    crate::markdown::reset_render_call_count();

    render_app(&app, 60, 20);
    let first_count = crate::markdown::render_call_count();
    assert!(first_count > 0, "should parse at least once");

    for _ in 0..5 {
        app.tick();
    }
    render_app(&app, 60, 20);
    let second_count = crate::markdown::render_call_count();

    assert_eq!(
        first_count, second_count,
        "typewriter ticks should not cause markdown re-parsing"
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

    render_app(&app, 60, 20);

    reset_indent_line_call_count();
    render_app(&app, 80, 20);
    let calls = indent_line_call_count();
    assert!(
        calls > 0,
        "width change should invalidate cache, got {calls} calls"
    );
}

#[test]
fn content_hash_cached_across_renders() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "hello world with some **markdown**",
    ));

    render_app(&app, 60, 20);
    reset_content_hash_calls();

    render_app(&app, 60, 20);
    let calls = content_hash_call_count();
    assert_eq!(
        calls, 0,
        "second render should use cached hash, got {calls} calls"
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

fn plain_style(theme: &crate::theme::Theme) -> MessageStyle<'_> {
    MessageStyle {
        theme,
        show_usage: false,
    }
}

fn rendered_text(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_rendered_message_outlives_the_app_it_came_from() {
    let theme = crate::theme::Theme::default();
    let lines: Vec<Line<'static>> = {
        let msg = DisplayMessage::new(MessageRole::Assistant, "hello world");
        message_lines(&msg, &plain_style(&theme), 80)
    };
    assert!(rendered_text(&lines).contains("hello world"));
}

#[test]
fn the_same_message_renders_the_same_lines() {
    let theme = crate::theme::Theme::default();
    let msg = DisplayMessage::new(MessageRole::Assistant, "one\n\ntwo\n\nthree");
    let first = message_lines(&msg, &plain_style(&theme), 60);
    let second = message_lines(&msg, &plain_style(&theme), 60);
    assert_eq!(rendered_text(&first), rendered_text(&second));
}

#[test]
fn a_system_message_gets_a_label() {
    let theme = crate::theme::Theme::default();
    let msg = DisplayMessage::new(MessageRole::System, "compacted");
    let lines = message_lines(&msg, &plain_style(&theme), 60);
    assert!(rendered_text(&lines).starts_with("system"));
}

#[test]
fn a_collapsed_trace_is_one_line() {
    let theme = crate::theme::Theme::default();
    let trace = |expanded: bool| {
        let mut msg = DisplayMessage::new(MessageRole::Assistant, "the answer");
        msg.thinking = Some("**Working it out**\n\nfirst i consider\nthen i decide".into());
        msg.thinking_expanded = expanded;
        rendered_text(&message_lines(&msg, &plain_style(&theme), 60))
    };

    let collapsed = trace(false);
    assert!(collapsed.contains("Working it out"), "{collapsed}");
    assert!(!collapsed.contains("then i decide"), "{collapsed}");

    let expanded = trace(true);
    assert!(expanded.contains("then i decide"), "{expanded}");
}

#[test]
fn a_queued_message_renders_dim() {
    let theme = crate::theme::Theme::default();
    let mut msg = DisplayMessage::new(MessageRole::User, "steer me");
    msg.queued = true;
    let lines = message_lines(&msg, &plain_style(&theme), 60);
    assert!(rendered_text(&lines).contains("steer me"));
    assert!(
        lines.iter().any(|l| l.style == theme.dim),
        "a queued bubble is dimmed, not boxed"
    );
}

#[test]
fn the_usage_line_is_gated_on_the_setting() {
    let theme = crate::theme::Theme::default();
    let mut msg = DisplayMessage::new(MessageRole::Assistant, "done");
    msg.usage = Some(Usage {
        input_tokens: mush_ai::types::TokenCount::new(100),
        output_tokens: mush_ai::types::TokenCount::new(50),
        ..Default::default()
    });

    let off = message_lines(&msg, &plain_style(&theme), 60);
    assert!(!rendered_text(&off).contains("tok"));

    let on = message_lines(
        &msg,
        &MessageStyle {
            theme: &theme,
            show_usage: true,
        },
        60,
    );
    assert!(rendered_text(&on).contains("150tok"));
}

#[test]
fn a_running_tool_defers_the_usage_line() {
    let theme = crate::theme::Theme::default();
    let mut msg = DisplayMessage::new(MessageRole::Assistant, "working");
    msg.usage = Some(Usage {
        input_tokens: mush_ai::types::TokenCount::new(100),
        output_tokens: mush_ai::types::TokenCount::new(50),
        ..Default::default()
    });
    msg.tool_calls.push(DisplayToolCall {
        name: "bash".into(),
        summary: "ls".into(),
        status: ToolCallStatus::Running,
        output_preview: None,
        image_data: None,
        batch: 1,
    });

    let lines = message_lines(
        &msg,
        &MessageStyle {
            theme: &theme,
            show_usage: true,
        },
        60,
    );
    assert!(!rendered_text(&lines).contains("150tok"));
}

fn live_text(app: &App, width: u16) -> String {
    rendered_text(&live_lines(app, width))
}

#[test]
fn an_idle_live_region_is_empty() {
    let app = App::new("test".into(), TokenCount::new(200_000));
    assert!(live_lines(&app, 80).is_empty());
}

#[test]
fn a_streaming_reply_shows_in_the_live_region() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("partial answ");
    for _ in 0..50 {
        app.tick_streaming_text();
    }
    assert!(live_text(&app, 80).contains("partial answ"));
}

#[test]
fn a_stream_with_nothing_yet_still_says_it_is_working() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    assert!(live_text(&app, 80).contains("working"));
}

#[test]
fn a_compacting_run_shows_a_label() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.compacting = true;
    assert!(live_text(&app, 80).contains("compacting"));
}

#[test]
fn queued_steering_stays_live() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("first");
    app.push_queued_message("wait, also this");
    assert!(live_text(&app, 80).contains("wait, also this"));
}

#[test]
fn a_turn_stays_live_until_the_next_one_starts() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("old question");
    app.start_streaming();
    app.push_text_delta("old answer");
    app.finish_streaming(None, None);

    let during = live_text(&app, 80);
    assert!(during.contains("old question"), "got:\n{during}");
    assert!(during.contains("old answer"), "got:\n{during}");

    app.push_user_message("new question");
    let after = live_text(&app, 80);
    assert!(!after.contains("old answer"), "got:\n{after}");
    assert!(after.contains("new question"), "got:\n{after}");
}

#[test]
fn a_recap_takes_the_last_bold_header() {
    assert_eq!(
        thinking_recap("**Weighing options**\n\nblah\n\n**Picking one**\n\nmore"),
        "Picking one"
    );
}

#[test]
fn a_recap_takes_a_markdown_heading_too() {
    assert_eq!(
        thinking_recap("## Checking the parser\n\nblah"),
        "Checking the parser"
    );
}

#[test]
fn a_recap_without_a_header_takes_the_first_line() {
    assert_eq!(
        thinking_recap("i need to work out what the user wants\nthen do it"),
        "i need to work out what the user wants"
    );
}

#[test]
fn a_recap_drops_emphasis_and_a_trailing_colon() {
    assert_eq!(thinking_recap("**Reading `edit.rs`:**"), "Reading edit.rs");
}

#[test]
fn a_message_without_thinking_gets_no_recap_row() {
    let theme = crate::theme::Theme::default();
    let msg = DisplayMessage::new(MessageRole::Assistant, "the answer");
    let text = rendered_text(&message_lines(&msg, &plain_style(&theme), 60));

    assert!(!text.contains("thinking"), "{text}");
}

#[test]
fn a_long_recap_stays_on_one_line() {
    let theme = crate::theme::Theme::default();
    let mut msg = DisplayMessage::new(MessageRole::Assistant, "the answer");
    msg.thinking = Some("xy".repeat(200));

    let text = rendered_text(&message_lines(&msg, &plain_style(&theme), 60));
    let rows = text.lines().filter(|l| l.contains("xy")).count();
    assert_eq!(rows, 1, "a collapsed trace is one row, got:\n{text}");
}

fn reveal(app: &mut App) {
    for _ in 0..200 {
        app.tick();
    }
}

#[test]
fn the_live_region_holds_the_turn_that_started_it() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("the question");
    app.start_streaming();
    app.push_text_delta("the reply");
    reveal(&mut app);

    let text = live_text(&app, 60);
    assert!(text.contains("the question"), "got:\n{text}");
    assert!(text.contains("the reply"), "got:\n{text}");
}

#[test]
fn queued_steering_sits_below_the_reply() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("the question");
    app.start_streaming();
    app.push_text_delta("the reply");
    app.messages.push(DisplayMessage {
        queued: true,
        ..DisplayMessage::new(MessageRole::User, "steering")
    });
    reveal(&mut app);

    let text = live_text(&app, 60);
    let reply = text.find("the reply").expect("reply should render");
    let steering = text.find("steering").expect("steering should render");
    assert!(reply < steering, "got:\n{text}");
}

fn drawn_conversation(count: usize) -> (App, crate::screen::Screen, Buffer) {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..count {
        app.push_user_message(format!("question {i}"));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("## answer {i}\n\nwith **bold** and `code` in it"),
        ));
    }
    let mut screen = crate::screen::Screen::new();
    let mut buf = Buffer::empty(Rect::new(0, 0, 100, 40));
    screen.render(&app, buf.area, &mut buf);
    (app, screen, buf)
}

#[test]
fn reading_an_unchanged_conversation_rebuilds_nothing() {
    let (mut app, mut screen, mut buf) = drawn_conversation(195);
    crate::input::handle_key(
        &mut app,
        crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('j')),
    );
    reset_message_build_count();

    screen.render(&app, buf.area, &mut buf);

    assert_eq!(message_build_count(), 0);
}
