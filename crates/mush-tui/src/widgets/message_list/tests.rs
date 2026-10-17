use super::*;
use crate::test_support::buffer_to_string;
use mush_ai::types::{Dollars, Usage};

fn all_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let style = MessageStyle::for_app(app);
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
        content.contains("compacting… ctrl+c cancels"),
        "expected 'compacting' label in chat, got:\n{content}"
    );
}

#[test]
fn inline_busy_rows_do_not_render_braille_spinners() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();

    let content = buffer_to_string(&render_app(&app, 40, 10));

    assert!(content.contains("waiting"));
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
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "ls -la".into(),
            status: ToolCallStatus::Running,
            output: None,
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
    msg.tool_calls[0].output = Some("ok".into());
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
                tool_call_id: "tc".into(),
                name: "bash".into(),
                summary: "ls -la".into(),
                status: ToolCallStatus::Done,
                output: Some("file1.txt\nfile2.txt".into()),
                image_data: None,
                batch: 1,
                bash_done: None,
                cell: None,
            },
            crate::app::DisplayToolCall {
                tool_call_id: "tc".into(),
                name: "read".into(),
                summary: "src/main.rs".into(),
                status: ToolCallStatus::Error,
                output: None,
                image_data: None,
                batch: 2,
                bash_done: None,
                cell: None,
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
            tool_call_id: "tc".into(),
            name: "edit".into(),
            summary: "src/main.rs".into(),
            status: ToolCallStatus::Done,
            output: Some("- println!(\"hello\")\n+ println!(\"world\")".into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "edit".into(),
            summary: "src/main.rs".into(),
            status: ToolCallStatus::Done,
            output: Some("- println!(\"hello\")\n+ println!(\"world\")".into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "cargo test".into(),
            status: ToolCallStatus::Done,
            output: None,
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
        tool_call_id: "tc".into(),
        name: name.into(),
        summary: format!("{name} summary"),
        status: ToolCallStatus::Done,
        output: None,
        image_data: None,
        batch: 1,
        bash_done: None,
        cell: None,
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
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "cargo check".into(),
            status: ToolCallStatus::Error,
            output: Some("error[E0063]: missing field".into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "read".into(),
            summary: long_path.into(),
            status: ToolCallStatus::Done,
            output: None,
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "noop".into(),
            summary: "ok".into(),
            status: ToolCallStatus::Done,
            output: Some(output.clone()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "read".into(),
            summary: "src/lib.rs".into(),
            status: ToolCallStatus::Done,
            output: Some(format!("L1: {}", "abcｶﾞ".repeat(4))),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "read".into(),
            summary: "src/main.rs".into(),
            status: ToolCallStatus::Done,
            output: Some(
                "L1:         fn main() {\n\
                 L2:             println!(\"hi\");\n\
                 L3:         }"
                    .into(),
            ),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "read".into(),
            summary: "src/main.rs".into(),
            status: ToolCallStatus::Done,
            output: Some("L1: fn main() {\nL2:     println!(\"hi\");\nL3: }".into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "for file in *.rs; do echo $file; done".into(),
            status: ToolCallStatus::Done,
            output: Some("done".into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "script".into(),
            status: ToolCallStatus::Done,
            output: Some("for file in *.rs; do echo $file; done".into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "cargo nextest run".into(),
            status: ToolCallStatus::Done,
            output: Some(long_json.into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
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
    let rows = wrap_styled_spans(vec![Span::raw(line)], 15);
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

    let truncated = truncate_text(&text, 2);

    assert_eq!(plain_text(&truncated), "ｶﾞ");
}

#[test]
fn truncate_text_reveals_by_chars_across_span_boundary_after_wide_char() {
    let text = Text::from(Line::from(vec![Span::raw("界"), Span::raw("ab")]));

    let truncated = truncate_text(&text, 2);

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
        detail: ChatDetail::Overview,
        pulse: 0,
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
    let text = rendered_text(&lines);
    assert!(text.contains("steering \u{b7} steer me"), "{text}");
    assert!(text.contains("to edit the queued message"), "{text}");
    assert!(
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|span| span.content == "steer me" && span.style == theme.dim),
        "the queued text is dim, the label is not"
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
            detail: ChatDetail::Overview,
            pulse: 0,
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
        tool_call_id: "tc".into(),
        name: "bash".into(),
        summary: "ls".into(),
        status: ToolCallStatus::Running,
        output: None,
        image_data: None,
        batch: 1,
        bash_done: None,
        cell: None,
    });

    let lines = message_lines(
        &msg,
        &MessageStyle {
            theme: &theme,
            show_usage: true,
            detail: ChatDetail::Overview,
            pulse: 0,
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
fn a_stream_with_nothing_yet_says_it_is_waiting_and_for_how_long() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream
        .start_at(std::time::Instant::now() - std::time::Duration::from_secs(7));
    assert!(live_text(&app, 80).contains("waiting · 7s"));
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

#[test]
fn a_token_rebuilds_only_the_message_it_landed_on() {
    let (mut app, mut screen, mut buf) = drawn_conversation(195);
    reset_message_build_count();

    app.messages
        .last_mut()
        .expect("message")
        .content
        .push_str(" checker");
    screen.render(&app, buf.area, &mut buf);

    assert_eq!(message_build_count(), 1);
    assert!(buffer_to_string(&buf).contains(" checker"));
}

#[test]
fn narrowing_the_terminal_rebuilds_every_message_once() {
    let (app, mut screen, _) = drawn_conversation(195);
    let mut narrow = Buffer::empty(Rect::new(0, 0, 80, 40));
    reset_message_build_count();

    screen.render(&app, narrow.area, &mut narrow);
    let first = message_build_count();
    reset_message_build_count();
    screen.render(&app, narrow.area, &mut narrow);

    assert_eq!(first, 390);
    assert_eq!(message_build_count(), 0);
}

#[test]
fn a_tool_result_rebuilds_only_the_message_that_owns_it() {
    let (mut app, mut screen, mut buf) = drawn_conversation(195);
    reset_message_build_count();

    app.messages
        .last_mut()
        .expect("message")
        .tool_calls
        .push(crate::app::DisplayToolCall {
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "cargo build".into(),
            status: ToolCallStatus::Done,
            output: Some("compiled in 4.2s".into()),
            image_data: None,
            batch: 0,
            bash_done: None,
            cell: None,
        });
    screen.render(&app, buf.area, &mut buf);

    assert_eq!(message_build_count(), 1);
    assert!(buffer_to_string(&buf).contains("compiled in 4.2s"));
}

#[test]
fn expanding_a_thinking_block_rebuilds_only_that_message() {
    let (mut app, mut screen, mut buf) = drawn_conversation(195);
    app.messages.get_mut(200).expect("message").thinking =
        Some("**weighing it up**\n\nthe long way round".into());
    screen.render(&app, buf.area, &mut buf);
    reset_message_build_count();

    app.messages
        .get_mut(200)
        .expect("message")
        .thinking_expanded = true;
    screen.render(&app, buf.area, &mut buf);

    assert_eq!(message_build_count(), 1);
}

#[test]
fn a_running_tool_call_renders_its_header() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            tool_call_id: "tc".into(),
            name: "bash".into(),
            summary: "ls -la".into(),
            status: ToolCallStatus::Running,
            output: None,
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "let me check")
    });

    let content = buffer_to_string(&render_app(&app, 50, 15));

    assert!(content.contains("bash"), "{content}");
    assert!(content.contains("running"), "{content}");
}

#[test]
fn a_user_image_renders_as_a_placeholder_line() {
    let png = {
        let mut buf = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(12, 7)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    };
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let mut message = DisplayMessage::new(MessageRole::User, "look at this");
    message.images.push(png);
    app.messages.push(message);

    let text = rendered_text(&all_lines(&app, 60));

    assert!(text.contains("[image 1: png 12x7]"), "{text}");
    assert!(text.contains("look at this"));
}

#[test]
fn an_unreadable_image_still_gets_a_placeholder() {
    assert_eq!(image_placeholder(2, b"not an image"), "[image 2: image]");
}

#[test]
fn the_code_preview_skips_imports_and_counts_the_rest() {
    let (preview, hidden) =
        code_preview("import os\n\n# scan\nfiles = list(os.scandir('.'))\nlen(files)");
    assert_eq!(preview, "files = list(os.scandir('.'))");
    assert_eq!(hidden, 4);

    let (only_import, hidden) = code_preview("import os");
    assert_eq!(only_import, "import os");
    assert_eq!(hidden, 0);
}

#[test]
fn an_ipython_cell_collapses_to_a_preview_and_expands_with_detail() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "on it"));
    app.start_streaming();
    app.start_tool(
        &mush_ai::types::ToolCallId::from("tc_1"),
        "ipython",
        "import re\nmatches = re.findall('x', text)\nlen(matches)",
    );
    app.end_tool(
        &mush_ai::types::ToolCallId::from("tc_1"),
        "ipython",
        mush_ai::types::ToolOutcome::Success,
        Some("3"),
        None,
    );

    let overview = rendered_text(&all_lines(&app, 80));
    assert!(
        overview.contains("\u{2713} python \u{b7} matches = re.findall('x', text) \u{b7} \u{2191} 3 \u{2193} 1 lines"),
        "{overview}"
    );
    assert!(!overview.contains("import re"), "{overview}");
    assert_eq!(overview.lines().filter(|l| l.contains("python")).count(), 1);

    app.detail = ChatDetail::Details;
    let details = rendered_text(&all_lines(&app, 80));
    assert!(!details.contains("import re"), "{details}");
    assert!(!details.contains("\u{203a} 3"), "{details}");

    app.detail = ChatDetail::All;
    let all = rendered_text(&all_lines(&app, 80));
    assert!(all.contains(" \u{2570}\u{2500} import re"), "{all}");
    assert!(all.contains("    len(matches)"), "{all}");
    assert!(all.contains("  \u{203a} 3"), "{all}");
}

fn cell_details() -> mush_tools::ipython::CellDetails {
    mush_tools::ipython::CellDetails {
        duration_ms: 1_234,
        stdout: "one\ntwo\n".into(),
        stderr: String::new(),
        result: Some("'edited /repo/a.txt'".into()),
        error: None,
        diffs: vec![mush_tools::ipython::CellDiff {
            path: "/repo/a.txt".into(),
            diff: "- hello world\n+ hello there".into(),
        }],
        images: 2,
        kernel_restarted: false,
        sent_messages: vec![],
        background: String::new(),
    }
}

fn cell_message(cell: mush_tools::ipython::CellDetails, status: ToolCallStatus) -> DisplayMessage {
    DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            tool_call_id: "call_1".into(),
            name: "ipython".into(),
            summary: "print('one')\nawait edit('/repo/a.txt', 'world', 'there')".into(),
            status,
            output: Some("one\ntwo\n'edited /repo/a.txt'".into()),
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: Some(cell),
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    }
}

#[test]
fn a_structured_cell_shows_duration_output_and_the_edit_summary_by_detail_level() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(cell_message(cell_details(), ToolCallStatus::Done));

    app.detail = ChatDetail::Overview;
    let overview = rendered_text(&all_lines(&app, 90));
    assert!(
        overview.contains("\u{2191} 2 \u{2193} 2 lines \u{b7} 1.2s"),
        "{overview}"
    );
    assert!(
        overview.contains("    \u{2570}\u{2500} /repo/a.txt +1 -1"),
        "{overview}"
    );
    assert!(!overview.contains("hello there"), "{overview}");
    assert!(!overview.contains("\u{203a} one"), "{overview}");

    app.detail = ChatDetail::Details;
    let details = rendered_text(&all_lines(&app, 90));
    assert!(details.contains("/repo/a.txt +1 -1"), "{details}");
    assert!(details.contains("hello there"), "{details}");
    assert!(!details.contains("await edit("), "{details}");
    assert!(!details.contains("\u{203a} one"), "{details}");
    assert!(!details.contains("images sent"), "{details}");

    app.detail = ChatDetail::All;
    let all = rendered_text(&all_lines(&app, 90));
    assert!(all.contains("    await edit("), "{all}");
    assert!(all.contains("/repo/a.txt +1 -1"), "{all}");
    assert!(all.contains("hello there"), "{all}");
    assert!(all.contains("  \u{203a} one"), "{all}");
    assert!(all.contains("    two"), "{all}");
    assert!(!all.contains("edited /repo/a.txt"), "{all}");
    assert!(all.contains("2 images sent to the model"), "{all}");
}

#[test]
fn a_full_cell_keeps_every_output_line() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let stdout: String = (1..=40).map(|n| format!("row {n}\n")).collect();
    app.messages.push(cell_message(
        mush_tools::ipython::CellDetails {
            stdout,
            ..cell_details()
        },
        ToolCallStatus::Done,
    ));

    app.detail = ChatDetail::All;
    let all = rendered_text(&all_lines(&app, 90));
    assert!(all.contains("  \u{203a} row 1\n"), "{all}");
    assert!(all.contains("row 20"), "{all}");
    assert!(all.contains("row 40"), "{all}");
}

#[test]
fn a_sent_agent_message_shows_as_a_row_and_hides_the_receipt_dict() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let cell = mush_tools::ipython::CellDetails {
        duration_ms: 40,
        stdout: String::new(),
        stderr: String::new(),
        result: Some(
            "{'deliveredAt': '2026-09-23T00:00:00.000Z', 'deliveryStatus': 'delivered', 'id': 'agentmsg_1'}"
                .into(),
        ),
        error: None,
        diffs: vec![],
        images: 0,
        kernel_restarted: false,
        sent_messages: vec![mush_tools::ipython::SentMessage {
            receiver: "auth".into(),
            status: "delivered".into(),
            text: "check the regression test too".into(),
        }],
        background: String::new(),
    };
    app.messages.push(cell_message(cell, ToolCallStatus::Done));

    for detail in [ChatDetail::Overview, ChatDetail::Details] {
        app.detail = detail;
        let text = rendered_text(&all_lines(&app, 90));
        assert!(
            text.contains(" \u{25c6} agent message sent \u{b7} auth (delivered)"),
            "{text}"
        );
        assert!(!text.contains("deliveryStatus"), "{text}");
        assert!(!text.contains("regression test"), "{text}");
    }

    app.detail = ChatDetail::All;
    let all = rendered_text(&all_lines(&app, 90));
    assert!(all.contains("agent message sent"), "{all}");
    assert!(
        all.contains(" \u{2570}\u{2500} check the regression test too"),
        "{all}"
    );
    assert!(!all.contains("receipts"), "{all}");
}

#[test]
fn a_failed_cell_names_the_error_on_top_and_keeps_the_traceback_for_details() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let cell = mush_tools::ipython::CellDetails {
        duration_ms: 12,
        stdout: String::new(),
        stderr: String::new(),
        result: None,
        error: Some(mush_tools::ipython::CellErrorDetails {
            ename: "ZeroDivisionError".into(),
            evalue: "division by zero".into(),
            traceback: vec![
                "Traceback (most recent call last):".into(),
                "  File \"<cell-1>\", line 1".into(),
                "ZeroDivisionError: division by zero".into(),
            ],
        }),
        diffs: vec![],
        images: 0,
        kernel_restarted: false,
        sent_messages: vec![],
        background: String::new(),
    };
    app.messages.push(cell_message(cell, ToolCallStatus::Error));

    let overview = rendered_text(&all_lines(&app, 90));
    assert!(overview.contains("\u{2717} python"), "{overview}");
    assert!(
        overview.contains("12ms \u{b7} ZeroDivisionError"),
        "{overview}"
    );
    assert!(!overview.contains("Traceback"), "{overview}");

    app.detail = ChatDetail::Details;
    let details = rendered_text(&all_lines(&app, 90));
    assert!(!details.contains("Traceback"), "{details}");

    app.detail = ChatDetail::All;
    let all = rendered_text(&all_lines(&app, 90));
    assert!(
        all.contains("  \u{203a} Traceback (most recent call last):"),
        "{all}"
    );
}

#[test]
fn a_running_cell_pulses_and_waits_for_output() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let mut cell = cell_message(cell_details(), ToolCallStatus::Running);
    cell.tool_calls[0].cell = None;
    cell.tool_calls[0].output = None;
    app.messages.push(cell);
    app.detail = ChatDetail::All;

    let first = rendered_text(&all_lines(&app, 90));
    assert!(first.contains("waiting for output..."), "{first}");
    for _ in 0..crate::widgets::working_icon::WORKING_ICON_TICKS {
        app.tick();
    }
    let second = rendered_text(&all_lines(&app, 90));

    let marker = |text: &str| {
        text.lines()
            .find(|l| l.contains("python"))
            .unwrap()
            .chars()
            .nth(1)
            .unwrap()
    };
    assert_ne!(marker(&first), marker(&second), "{first}\n{second}");
}

fn bash_done(exit_code: i32) -> mush_tools::kernel_jobs::BashDone {
    mush_tools::kernel_jobs::BashDone {
        pid: Some(4242),
        exit_code,
        command: "sleep 20".into(),
    }
}

#[test]
fn an_attached_bash_done_shows_only_when_the_cell_is_expanded() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage {
        tool_calls: vec![crate::app::DisplayToolCall {
            tool_call_id: "call_9".into(),
            name: "ipython".into(),
            summary: "h = bash('sleep 20')".into(),
            status: ToolCallStatus::Done,
            output: None,
            image_data: None,
            batch: 1,
            bash_done: Some(bash_done(0)),
            cell: None,
        }],
        ..DisplayMessage::new(MessageRole::Assistant, "")
    });

    for detail in [ChatDetail::Overview, ChatDetail::Details] {
        app.detail = detail;
        let text = buffer_to_string(&render_app(&app, 70, 8));
        assert!(!text.contains("background shell command"), "{text}");
    }

    app.detail = ChatDetail::All;
    let all = buffer_to_string(&render_app(&app, 70, 8));
    assert!(
        all.contains("background shell command finished · pid 4242"),
        "{all}"
    );
}

#[test]
fn a_detached_bash_done_is_one_line_and_names_the_exit_code_on_failure() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_bash_done(&bash_done(0), None);
    app.push_bash_done(&bash_done(5), None);

    let text = buffer_to_string(&render_app(&app, 80, 8));

    assert!(
        text.contains("ok background shell command finished · sleep 20"),
        "{text}"
    );
    assert!(
        text.contains("x background shell command failed · exit 5 · sleep 20"),
        "{text}"
    );
    assert!(!text.contains("bash-done"), "{text}");
    assert_eq!(
        message_lines(&app.messages[0], &MessageStyle::for_app(&app), 80).len(),
        1
    );
}

#[test]
fn a_status_flushes_into_a_dim_transcript_line_and_the_next_one_replaces_it() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("hi");
    app.status = Some("aborted".into());
    app.flush_status();
    app.status = Some("compacted".into());
    app.flush_status();

    assert_eq!(app.messages.len(), 2);
    let text = buffer_to_string(&render_app(&app, 60, 6));
    assert!(text.contains("compacted"), "{text}");
    assert!(!text.contains("aborted"), "{text}");
    assert!(!text.contains("system"), "{text}");
}

#[test]
fn an_error_gets_its_own_line_and_a_later_status_keeps_it() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("hi");
    app.status = Some("aborted".into());
    app.show_error("clipboard failed: wl-copy");
    app.status = Some("detail: overview".into());
    app.flush_status();

    let roles: Vec<_> = app.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Status { error: false },
            MessageRole::Status { error: true },
            MessageRole::Status { error: false },
        ]
    );
    let text = buffer_to_string(&render_app(&app, 60, 12));
    assert!(text.contains("aborted"), "{text}");
    assert!(
        text.contains("\u{26a0} clipboard failed: wl-copy"),
        "{text}"
    );
    assert!(text.contains("detail: overview"), "{text}");
}

#[test]
fn a_status_after_a_new_message_gets_its_own_line() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.status = Some("compacted".into());
    app.flush_status();
    app.push_user_message("next");
    app.status = Some("aborted".into());
    app.flush_status();

    let roles: Vec<_> = app.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::Status { error: false },
            MessageRole::User,
            MessageRole::Status { error: false },
        ]
    );
}

fn agent_message_event() -> mush_ai::types::EventMessage {
    use mush_ai::types::{EventOrigin, EventReceipt, Timestamp};
    mush_ai::types::EventMessage {
        content: "[agent-message from child:auth]\n\nfound the bug".into(),
        timestamp_ms: Timestamp::zero(),
        kind: mush_ai::types::EventKind::Message,
        receipts: vec![EventReceipt {
            id: String::from("agent-message-1").into(),
            timestamp_ms: Timestamp::zero(),
            kind: mush_ai::types::EventKind::Message,
            origin: EventOrigin::Agent {
                session_id: "s".into(),
                name: "auth".into(),
                relationship: "child".into(),
            },
        }],
    }
}

#[test]
fn a_received_agent_message_shows_its_sender_and_body_with_receipts_only_in_all() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let mut event = agent_message_event();
    event.content = "[agent-message from child:auth]\n\nfound **the bug** in `login.rs`".into();
    app.push_event_message(&event);

    for detail in [ChatDetail::Overview, ChatDetail::Details] {
        app.detail = detail;
        let text = buffer_to_string(&render_app(&app, 80, 10));
        assert!(text.contains("\u{25c6} message from child:auth"), "{text}");
        assert!(text.contains("found the bug in login.rs"), "{text}");
        assert!(!text.contains("agent-message"), "{text}");
        assert!(!text.contains("receipt"), "{text}");
        assert!(!text.contains("\"receipts\""), "{text}");
    }

    app.detail = ChatDetail::All;
    let text = buffer_to_string(&render_app(&app, 80, 20));
    assert!(text.contains("found the bug in login.rs"), "{text}");
    assert!(
        text.contains("receipt agent-message-1 · child auth"),
        "{text}"
    );
    assert!(!text.contains("\"content\""), "{text}");
}

#[test]
fn a_steered_agent_message_renders_like_a_delivered_one() {
    let event = agent_message_event();
    let mut steered = App::new("test".into(), TokenCount::new(200_000));
    crate::conversation_display::rebuild_display(
        &mut steered,
        &[crate::test_support::user_message(&event.text())],
    );
    let mut delivered = App::new("test".into(), TokenCount::new(200_000));
    delivered.push_event_message(&event);

    let steered_text = buffer_to_string(&render_app(&steered, 80, 10));
    assert_eq!(
        steered_text,
        buffer_to_string(&render_app(&delivered, 80, 10))
    );
    assert!(
        steered_text.contains("message from child:auth"),
        "{steered_text}"
    );
    assert!(!steered_text.contains("non-human event"), "{steered_text}");
    assert_eq!(
        steered.messages[0].role,
        MessageRole::Event(mush_ai::types::EventKind::Message)
    );
}

fn agent_message_with(body: &str) -> mush_ai::types::EventMessage {
    let mut event = agent_message_event();
    event.content = format!("[agent-message from child:tidy-repos]\n\n{body}");
    event.receipts[0].origin = mush_ai::types::EventOrigin::Agent {
        session_id: "s".into(),
        name: "tidy-repos".into(),
        relationship: "child".into(),
    };
    event
}

fn row_texts(buf: &Buffer) -> Vec<String> {
    buffer_to_string(buf)
        .lines()
        .map(|row| row.trim_end().to_string())
        .collect()
}

#[test]
fn an_agent_message_keeps_every_line_its_sender_wrote() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.detail = ChatDetail::All;
    app.push_event_message(&agent_message_with(
        "UNMERGED WORK, needs merge or drop\n| # | repo | workspace / change |\n| 1 | ai-safety | dangling urvxxukstpys |\nchecks for each one:\n    the owner was not working\n\n- mush: mush-models 23.4 GB\n- ai-safety: map-gleam 3.7",
    ));

    let rows = row_texts(&render_app(&app, 80, 14));

    for expected in [
        " \u{258e} UNMERGED WORK, needs merge or drop",
        " \u{258e} | # | repo | workspace / change |",
        " \u{258e} | 1 | ai-safety | dangling urvxxukstpys |",
        " \u{258e} checks for each one:",
        " \u{258e}     the owner was not working",
    ] {
        assert!(
            rows.iter().any(|row| row == expected),
            "{expected:?} in {rows:#?}"
        );
    }
    for item in ["mush: mush-models 23.4 GB", "ai-safety: map-gleam 3.7"] {
        assert!(
            rows.iter()
                .any(|row| row.ends_with(item) && row.starts_with(" \u{258e} ")),
            "{item:?} in {rows:#?}"
        );
    }
}

#[test]
fn an_agent_message_carries_a_gutter_in_its_senders_colour_and_the_assistant_none() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "my own words"));
    app.push_event_message(&agent_message_with("a child's words\nand a second line"));

    let buf = render_app(&app, 60, 8);
    let rows = row_texts(&buf);

    let own = rows
        .iter()
        .position(|row| row.contains("my own words"))
        .unwrap();
    assert!(!rows[own].contains('\u{258e}'), "{rows:#?}");
    let header = rows
        .iter()
        .position(|row| row.contains("message from child:tidy-repos"))
        .unwrap();
    let sender = sender_style(&app.theme, "child:tidy-repos").fg;
    assert!(sender.is_some());
    for row in header + 1..header + 3 {
        assert_eq!(buf[(1, row as u16)].symbol(), "\u{258e}", "{rows:#?}");
        assert_eq!(buf[(1, row as u16)].fg, sender.unwrap());
    }
    let name_at = rows[header].find("child:tidy-repos").unwrap();
    let column = rows[header][..name_at].chars().count() as u16;
    assert_eq!(buf[(column, header as u16)].fg, sender.unwrap());
}

#[test]
fn senders_keep_their_colour_and_different_senders_can_differ() {
    let theme = Theme::default();
    assert_eq!(
        sender_style(&theme, "child:auth"),
        sender_style(&theme, "child:auth")
    );
    let colours: std::collections::HashSet<_> =
        ["child:a", "child:b", "child:c", "child:d", "parent:root"]
            .iter()
            .map(|who| sender_style(&theme, who).fg)
            .collect();
    assert!(colours.len() > 1, "{colours:?}");
}

#[test]
fn a_long_agent_message_shows_its_first_rows_until_it_is_opened_or_all_output_is_on() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let body: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
    app.push_event_message(&agent_message_with(&body.join("\n")));

    for detail in [ChatDetail::Overview, ChatDetail::Details] {
        app.detail = detail;
        let rows = row_texts(&render_app(&app, 60, 40));
        let shown: Vec<&String> = rows.iter().filter(|row| row.contains("line ")).collect();
        assert_eq!(shown.len(), 5, "{rows:#?}");
        assert!(
            rows.iter()
                .any(|row| row == " \u{258e} \u{2026} 25 more lines, ctrl+o then enter to open"),
            "{rows:#?}"
        );
    }

    app.detail = ChatDetail::All;
    let rows = row_texts(&render_app(&app, 60, 40));
    assert_eq!(rows.iter().filter(|row| row.contains("line ")).count(), 30);
    assert!(!rows.iter().any(|row| row.contains("more lines")));

    app.detail = ChatDetail::Details;
    if let Some(mut message) = app.messages.last_mut() {
        message.expanded = true;
    }
    let rows = row_texts(&render_app(&app, 60, 40));
    assert_eq!(rows.iter().filter(|row| row.contains("line ")).count(), 30);
}

#[test]
fn blank_rows_inside_a_folded_message_do_not_use_up_its_preview() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let body: Vec<String> = (1..=12).map(|n| format!("paragraph {n}")).collect();
    app.push_event_message(&agent_message_with(&body.join("\n\n")));

    let rows = row_texts(&render_app(&app, 60, 40));

    let shown: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains("paragraph "))
        .collect();
    assert_eq!(shown.len(), 5, "{rows:#?}");
    let marker = rows
        .iter()
        .position(|row| row.contains("7 more lines"))
        .unwrap();
    assert!(rows[marker - 1].contains("paragraph 5"), "{rows:#?}");
}

#[test]
fn a_message_one_row_over_the_clip_is_shown_whole() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let body: Vec<String> = (1..=6).map(|n| format!("line {n}")).collect();
    app.push_event_message(&agent_message_with(&body.join("\n")));

    let rows = row_texts(&render_app(&app, 60, 12));

    assert_eq!(rows.iter().filter(|row| row.contains("line ")).count(), 6);
    assert!(!rows.iter().any(|row| row.contains("more lines")));
}

#[test]
fn a_child_exit_notice_names_the_child_and_what_happened() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let mut event = agent_message_event();
    event.content = "[child-failed child:auth]\n\nprovider error".into();
    app.push_event_message(&event);

    let text = buffer_to_string(&render_app(&app, 80, 10));
    assert!(text.contains("\u{25c6} child:auth failed"), "{text}");
    assert!(text.contains("provider error"), "{text}");
}

#[test]
fn a_control_message_keeps_its_full_body() {
    use mush_ai::types::{EventOrigin, EventReceipt, Timestamp};
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let mut event = agent_message_event();
    event.receipts = vec![EventReceipt {
        id: String::from("control-1").into(),
        timestamp_ms: Timestamp::zero(),
        kind: mush_ai::types::EventKind::Message,
        origin: EventOrigin::Legacy,
    }];
    app.push_event_message(&event);

    let text = buffer_to_string(&render_app(&app, 80, 20));

    assert!(text.contains("\u{25c6} non-human event"), "{text}");
    assert!(text.contains("[agent-message from child:auth]"), "{text}");
    assert!(text.contains("found the"), "{text}");
    assert!(!text.contains("\u{25c6} message from child:auth"), "{text}");
}

#[test]
fn harness_prompts_render_as_dim_labelled_rows_not_user_boxes() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    crate::conversation_display::rebuild_display(
        &mut app,
        &[
            crate::test_support::user_message("[heartbeat: every 20s run#3]\n\ncheck the tests"),
            crate::test_support::user_message(
                "[goal: continuation 2]\n\nContinue working toward the goal.",
            ),
        ],
    );
    assert!(app.messages.iter().all(|m| m.role == MessageRole::Injected));

    let text = buffer_to_string(&render_app(&app, 60, 12));
    assert!(
        text.contains("\u{25c6} heartbeat · every 20s · run 3"),
        "{text}"
    );
    assert!(text.contains("check the tests"), "{text}");
    assert!(text.contains("\u{25c6} goal · continuation 2"), "{text}");
    assert!(!text.contains("Continue working"), "{text}");
    assert!(!text.contains("you"), "{text}");

    app.detail = ChatDetail::All;
    let text = buffer_to_string(&render_app(&app, 60, 12));
    assert!(text.contains("Continue working toward the goal."), "{text}");
}

#[test]
fn a_compaction_shows_a_two_line_preview_until_all_output() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let summary = format!(
        "## Goal\n\n{}",
        "the parser rewrite keeps going ".repeat(12)
    );
    let mut row = crate::injected::Injected::compaction(&summary);
    row.details.push("24 → 6 messages".into());
    app.push_injected(MessageRole::Event(mush_ai::types::EventKind::Summary), row);

    let text = buffer_to_string(&render_app(&app, 50, 12));
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let header = rows
        .iter()
        .position(|line| line.contains("\u{25c6} context compacted"))
        .unwrap();
    assert!(
        rows[header + 1].contains("## Goal the parser rewrite"),
        "{text}"
    );
    assert!(rows[header + 2].trim_end().ends_with('\u{2026}'), "{text}");
    assert!(!text.contains("24 → 6 messages"), "{text}");

    app.detail = ChatDetail::All;
    let text = buffer_to_string(&render_app(&app, 50, 20));
    assert!(text.contains("Goal"), "{text}");
    assert!(!text.contains("## Goal"), "{text}");
    assert!(text.contains("24 → 6 messages"), "{text}");
}

#[test]
fn hidden_thinking_names_itself_with_its_own_clock() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let start = std::time::Instant::now() - std::time::Duration::from_secs(45);
    app.stream
        .start_at(start - std::time::Duration::from_secs(3));
    app.stream
        .begin_at(crate::turn_activity::StreamPart::Thinking, start);
    app.stream.push_thinking_at("", start);

    let text = live_text(&app, 80);
    assert!(text.contains("thinking · 45s"), "{text}");
    assert!(!text.contains("waiting"), "{text}");
}
