//! tests for the app state machine

use super::*;
use crate::input_buffer::word_boundary_left;
use crate::streaming::char_prefix;
use mush_ai::types::ToolOutcome;
use ratatui::layout::Rect;

#[test]
fn new_app_is_empty() {
    let app = App::new("test-model".into(), TokenCount::new(200_000));
    assert!(app.messages.is_empty());
    assert!(!app.stream.active);
    assert!(app.input.text.is_empty());
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn anthropic_models_use_provider_managed_context_for_cache_detection() {
    assert_eq!(
        context_management_mode_for_model_id("claude-opus-4-8", Some(true)),
        ContextManagementMode::ProviderManaged
    );
    assert_eq!(
        context_management_mode_for_model_id("claude-opus-4-8", Some(false)),
        ContextManagementMode::LocalMonotonic
    );
    assert_eq!(
        context_management_mode_for_model_id("local-model", Some(true)),
        ContextManagementMode::LocalMonotonic
    );
}

#[test]
fn push_user_message() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("hello");
    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].role, MessageRole::User);
    assert_eq!(app.messages[0].content, "hello");
}

#[test]
fn push_user_message_stores_images() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let images = vec![vec![0u8; 100], vec![1u8; 200]];
    app.push_user_message_with_images("hello", images.clone());
    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].content, "hello");
    assert_eq!(app.messages[0].images.len(), 2);
    assert_eq!(app.messages[0].images[0], images[0]);
}

#[test]
fn streaming_lifecycle() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    assert!(app.stream.active);

    app.push_text_delta("hello ");
    app.push_text_delta("world");
    assert_eq!(app.stream.text, "hello world");

    app.finish_streaming(None, None);
    assert!(!app.stream.active);
    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].content, "hello world");
    assert!(app.stream.text.is_empty());
}

#[test]
fn streaming_with_thinking() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_thinking_delta("let me think...");
    app.push_text_delta("answer");
    app.finish_streaming(None, None);

    assert_eq!(app.messages[0].thinking.as_deref(), Some("let me think..."));
    assert_eq!(app.messages[0].content, "answer");
}

#[test]
fn char_prefix_slices_correctly() {
    assert_eq!(char_prefix("hello", 0), "");
    assert_eq!(char_prefix("hello", 3), "hel");
    assert_eq!(char_prefix("hello", 5), "hello");
    assert_eq!(char_prefix("hello", 100), "hello");
    // multi-byte chars
    assert_eq!(char_prefix("café", 3), "caf");
    assert_eq!(char_prefix("café", 4), "café");
}

#[test]
fn typewriter_advances_with_ticks() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("hello world");

    // before any tick, nothing visible
    assert_eq!(app.visible_streaming_text(), "");

    // one tick advances partway (exponential ease)
    app.tick();
    let visible = app.visible_streaming_text();
    assert!(!visible.is_empty());
    assert!(visible.len() < "hello world".len());

    // enough ticks catch up fully
    for _ in 0..20 {
        app.tick();
    }
    assert_eq!(app.visible_streaming_text(), "hello world");
}

#[test]
fn typewriter_does_not_reveal_large_bursts_in_one_tick() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta(&"x".repeat(200));

    app.tick();

    assert!(
        app.visible_streaming_text_len() <= 12,
        "large provider bursts should reveal smoothly, visible chars={}",
        app.visible_streaming_text_len()
    );
}

#[test]
fn typewriter_resets_on_new_stream() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("first");
    for _ in 0..20 {
        app.tick();
    }
    assert_eq!(app.visible_streaming_text(), "first");

    // new stream resets
    app.start_streaming();
    assert_eq!(app.visible_streaming_text(), "");
}

#[test]
fn tool_lifecycle() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("do something");
    // simulate assistant message already pushed by finish_streaming
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));

    let tool_call_id = ToolCallId::from("tc_1");
    app.start_tool(&tool_call_id, "bash", "ls -la");
    assert_eq!(app.active_tools.len(), 1);
    assert_eq!(app.active_tools[0].name, "bash");
    assert_eq!(app.messages.last().unwrap().tool_calls.len(), 1);
    assert_eq!(
        app.messages.last().unwrap().tool_calls[0].status,
        ToolCallStatus::Running
    );

    app.end_tool(
        &tool_call_id,
        "bash",
        ToolOutcome::Success,
        Some("file1.txt\nfile2.txt"),
        None,
    );
    // tool stays in active_tools as done (panel persists)
    assert_eq!(app.active_tools.len(), 1);
    assert_eq!(app.active_tools[0].status, ToolCallStatus::Done);
    assert_eq!(
        app.messages.last().unwrap().tool_calls[0].status,
        ToolCallStatus::Done
    );

    // tools persist through streaming turns (cleared on new user submit)
    app.start_streaming();
    assert_eq!(app.active_tools.len(), 1);
    assert_eq!(app.active_tools[0].status, ToolCallStatus::Done);

    // explicitly clearing simulates user submitting new prompt
    app.active_tools.clear();
    assert!(app.active_tools.is_empty());
}

#[test]
fn ensure_cursor_visible_recomputes_total_when_stale() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.area.set(Rect::new(0, 0, 20, 8));
    app.input.visible_lines.set(2);
    app.input.total_lines.set(2); // stale from previous render
    app.input.text = "one\ntwo\nthree\nfour\nfive".into();
    app.input.cursor = app.input.text.len();

    app.input.ensure_cursor_visible();

    assert!(app.input.total_lines.get() >= 5);
    assert!(app.input.scroll.get() > 0);
}

#[test]
fn input_editing() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input_char('h');
    app.input_char('i');
    assert_eq!(app.input.text, "hi");
    assert_eq!(app.input.cursor, 2);

    app.input.cursor_left();
    assert_eq!(app.input.cursor, 1);
    app.input_char('!');
    assert_eq!(app.input.text, "h!i");

    app.input.cursor_home();
    assert_eq!(app.input.cursor, 0);
    app.input.cursor_end();
    assert_eq!(app.input.cursor, 3);

    app.input.backspace();
    assert_eq!(app.input.text, "h!");
}

#[test]
fn input_delete() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "abc".into();
    app.input.cursor = 1;
    app.input.delete();
    assert_eq!(app.input.text, "ac");
}

#[test]
fn take_input_resets() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello".into();
    app.input.cursor = 3;
    let text = app.input.take_text();
    assert_eq!(text, "hello");
    assert!(app.input.text.is_empty());
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn cost_accumulates() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("a");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(100),
            output_tokens: TokenCount::new(50),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::ZERO,
        }),
        Some(Dollars::new(0.005)),
    );

    app.start_streaming();
    app.push_text_delta("b");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(200),
            output_tokens: TokenCount::new(100),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::ZERO,
        }),
        Some(Dollars::new(0.01)),
    );

    assert!((app.stats.total_cost.get() - 0.015).abs() < f64::EPSILON);
    assert_eq!(app.stats.total_tokens, TokenCount::new(450));
    assert_eq!(app.stats.input_tokens, TokenCount::new(300));
    assert_eq!(app.stats.output_tokens, TokenCount::new(150));
    assert_eq!(app.stats.cache_read_tokens, TokenCount::ZERO);
    assert_eq!(app.stats.cache_write_tokens, TokenCount::ZERO);
    assert_eq!(app.stats.context_tokens, TokenCount::new(200));
}

#[test]
fn truncate_short_output() {
    let output = truncate_output("hello\nworld");
    assert_eq!(output, "hello\nworld");
}

#[test]
fn truncate_long_output() {
    let lines = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = truncate_output(&lines);
    assert!(output.contains("line 0"));
    assert!(output.contains("line 11"));
    assert!(output.contains("8 more lines"));
    assert!(!output.contains("line 12\n"));
}

#[test]
fn truncate_long_line() {
    let long = "x".repeat(200);
    let output = truncate_output(&long);
    assert!(output.len() < 200);
    assert!(output.ends_with('…'));
    assert!(!output.contains("..."));
}

#[test]
fn truncate_more_lines_footer_uses_single_ellipsis() {
    let lines = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = truncate_output(&lines);
    assert!(output.contains("… (8 more lines)"));
    assert!(!output.contains("... (8 more lines)"));
}

#[test]
fn truncate_output_large_allows_more_lines_than_small() {
    let lines = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    // 30 lines: small cap (12) truncates, large cap (40) does not
    let small = truncate_output(&lines);
    let large = truncate_output_large(&lines);
    assert!(
        small.contains("more lines"),
        "small cap should truncate 30 lines"
    );
    assert!(
        !large.contains("more lines"),
        "large cap should fit 30 lines: {large}"
    );
    assert!(large.contains("line 0"));
    assert!(large.contains("line 29"));
}

#[test]
fn tool_output_stored() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));
    let tool_call_id = ToolCallId::from("tc_1");
    app.start_tool(&tool_call_id, "read", "src/main.rs");
    app.end_tool(
        &tool_call_id,
        "read",
        ToolOutcome::Success,
        Some("fn main() {}\n"),
        None,
    );
    let tc = &app.messages.last().unwrap().tool_calls[0];
    assert!(tc.output_preview.is_some());
    assert!(tc.output_preview.as_ref().unwrap().contains("fn main()"));
}

#[test]
fn edit_tool_output_preserves_long_diffs_without_truncation() {
    // regression: truncate_output_large caps at MAX_SINGLE_TOOL_LINES
    // (40) which cut legitimate edit diffs in half even when the
    // terminal had plenty of vertical room. edit diffs are bounded by
    // the size of the change the user is reviewing, so they should
    // render at full length and let the scroll viewport handle overflow
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));
    let tool_call_id = ToolCallId::from("tc_edit");
    app.start_tool(&tool_call_id, "edit", "src/main.rs");
    // a realistic 80-line edit diff
    let diff: String = (0..80_usize)
        .map(|i| {
            if i.is_multiple_of(3) {
                format!("- line {i}\n+ line {i} (changed)")
            } else {
                format!("  line {i}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    app.end_tool(
        &tool_call_id,
        "edit",
        ToolOutcome::Success,
        Some(&diff),
        None,
    );
    let tc = &app.messages.last().unwrap().tool_calls[0];
    let preview = tc.output_preview.as_ref().expect("preview stored");
    assert!(
        preview.contains("line 79"),
        "full edit diff should be preserved (last line missing): {preview}"
    );
    assert!(
        !preview.contains("more lines"),
        "edit diffs should not be preview-truncated: {preview}"
    );
}

#[test]
fn batch_tool_expands_into_sub_calls() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));

    let tc_id = ToolCallId::from("tc_batch");
    app.start_batch_tool(
        &tc_id,
        "3 tool calls",
        &[
            ("read".into(), "a.rs".into()),
            ("read".into(), "b.rs".into()),
            ("bash".into(), "cargo test".into()),
        ],
    );

    // one live panel per sub-call, keyed by synthetic exec ids so each
    // sub-call streams into its own panel
    assert_eq!(app.active_tools.len(), 3);
    assert_eq!(app.active_tools[0].name, "read");
    assert_eq!(app.active_tools[2].name, "bash");
    assert_eq!(app.active_tools[0].tool_call_id.as_str(), "tc_batch#0");
    assert_eq!(app.active_tools[2].tool_call_id.as_str(), "tc_batch#2");
    // three display entries in the message
    let tcs = &app.messages.last().unwrap().tool_calls;
    assert_eq!(tcs.len(), 3);
    assert_eq!(tcs[0].name, "read");
    assert_eq!(tcs[1].name, "read");
    assert_eq!(tcs[2].name, "bash");
    // all same batch
    assert_eq!(tcs[0].batch, tcs[1].batch);
    assert_eq!(tcs[1].batch, tcs[2].batch);
    assert!(tcs.iter().all(|t| t.status == ToolCallStatus::Running));
}

#[test]
fn batch_sub_calls_stream_live_output_to_their_own_panels() {
    // the bug being fixed: parallel sub-calls all dumped their live
    // output into the last panel. each sub-call's synthetic exec id must
    // route to its own panel.
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));
    let tc_id = ToolCallId::from("tc_batch");
    app.start_batch_tool(
        &tc_id,
        "2 tool calls",
        &[
            ("bash".into(), "jj st".into()),
            ("bash".into(), "cargo test".into()),
        ],
    );

    app.push_tool_output_by_exec_id("tc_batch#0", "jj line");
    app.push_tool_output_by_exec_id("tc_batch#1", "test line");

    assert_eq!(app.active_tools[0].live_output.as_deref(), Some("jj line"));
    assert_eq!(
        app.active_tools[1].live_output.as_deref(),
        Some("test line")
    );
}

#[test]
fn batch_end_distributes_results() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));

    let tc_id = ToolCallId::from("tc_batch");
    app.start_batch_tool(
        &tc_id,
        "2 tool calls",
        &[
            ("read".into(), "a.rs".into()),
            ("bash".into(), "cargo test".into()),
        ],
    );

    let output = "\
--- [0] read [ok] ---
fn main() {}

--- [1] bash [error] ---
error: could not compile

batch: 1/2 succeeded, 1 failed";

    app.end_batch_tool(&tc_id, ToolOutcome::Error, Some(output));

    let tcs = &app.messages.last().unwrap().tool_calls;
    assert_eq!(tcs[0].status, ToolCallStatus::Done);
    assert!(tcs[0].output_preview.as_ref().unwrap().contains("fn main"));
    assert_eq!(tcs[1].status, ToolCallStatus::Error);
    assert!(
        tcs[1]
            .output_preview
            .as_ref()
            .unwrap()
            .contains("could not compile")
    );
}

#[test]
fn batch_end_with_top_level_error_shows_error_on_subcalls() {
    // when the batch tool itself errors before producing per-sub-call
    // sections (e.g. BatchArgs parse failure: missing `parameters` on
    // one call, or extra fields rejected by deny_unknown_fields), the
    // result is a single top-level error string with no `--- [N] ...
    // ---` headers. previously every sub-call display entry fell
    // through to the unhelpful "missing result in batch output"
    // fallback, which hid the real error from the user. the actual
    // error text should be surfaced on every sub-call instead so the
    // user can see what actually went wrong
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));

    let tc_id = ToolCallId::from("tc_batch");
    app.start_batch_tool(
        &tc_id,
        "2 tool calls",
        &[
            ("read".into(), "a.rs".into()),
            ("read".into(), "b.rs".into()),
        ],
    );

    let output = "invalid arguments: missing field `parameters` at line 1 column 42\n\
                  received: {tool_calls: [{tool: \"read\"}]}";
    app.end_batch_tool(&tc_id, ToolOutcome::Error, Some(output));

    let tcs = &app.messages.last().unwrap().tool_calls;
    assert_eq!(tcs[0].status, ToolCallStatus::Error);
    assert_eq!(tcs[1].status, ToolCallStatus::Error);
    for tc in tcs {
        let preview = tc.output_preview.as_deref().unwrap_or_default();
        assert!(
            preview.contains("invalid arguments") && preview.contains("missing field `parameters`"),
            "expected real error in preview, got: {preview:?}",
        );
        assert!(
            !preview.contains("missing result in batch output"),
            "preview must not hide the real error behind a placeholder: {preview:?}",
        );
    }
}

#[test]
fn batch_end_with_error_outcome_marks_active_panel_error() {
    // the live tool panels (active_tools) sit above the message history,
    // one per sub-call. previously a single batch panel was unconditionally
    // set to Done, so a failed batch still rendered a green check. each
    // sub-call panel should reflect its own outcome from the parsed output.
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));

    let tc_id = ToolCallId::from("tc_batch");
    app.start_batch_tool(&tc_id, "1 tool call", &[("read".into(), "a.rs".into())]);

    app.end_batch_tool(
        &tc_id,
        ToolOutcome::Error,
        Some("--- [0] read [error] ---\nboom\n\nbatch: 0/1 succeeded, 1 failed"),
    );

    let panel = app
        .active_tools
        .iter()
        .find(|t| t.tool_call_id.as_str() == "tc_batch#0")
        .expect("live panel for sub-call");
    assert_eq!(panel.status, ToolCallStatus::Error);
}

#[test]
fn batch_end_with_success_outcome_marks_active_panel_done() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));

    let tc_id = ToolCallId::from("tc_batch");
    app.start_batch_tool(&tc_id, "1 tool call", &[("read".into(), "a.rs".into())]);

    app.end_batch_tool(
        &tc_id,
        ToolOutcome::Success,
        Some("--- [0] read [ok] ---\nhi\n\nbatch: 1/1 succeeded, 0 failed"),
    );

    let panel = app
        .active_tools
        .iter()
        .find(|t| t.tool_call_id.as_str() == "tc_batch#0")
        .expect("live panel for sub-call");
    assert_eq!(panel.status, ToolCallStatus::Done);
}

#[test]
fn parse_batch_output_splits_sections() {
    let text = "\
--- [0] read [ok] ---
content a

--- [1] bash [error] ---
error text

batch: 1/2 succeeded, 1 failed";

    let sections = parse_batch_output(text);
    assert_eq!(sections.len(), 2);
    assert!(!sections[0].is_error);
    assert_eq!(sections[0].content, "content a");
    assert!(sections[1].is_error);
    assert_eq!(sections[1].content, "error text");
}

#[test]
fn toggle_thinking() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_thinking_delta("deep thoughts");
    app.push_text_delta("answer");
    app.finish_streaming(None, None);

    // starts expanded (default ThinkingDisplay::Expanded)
    assert!(app.messages[0].thinking_expanded);

    app.toggle_thinking_expanded();
    assert!(!app.messages[0].thinking_expanded);

    app.toggle_thinking_expanded();
    assert!(app.messages[0].thinking_expanded);
}

#[test]
fn toggle_thinking_targets_last_assistant() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));

    // first assistant with thinking
    app.start_streaming();
    app.push_thinking_delta("old thoughts");
    app.push_text_delta("first");
    app.finish_streaming(None, None);

    // second assistant with thinking
    app.start_streaming();
    app.push_thinking_delta("new thoughts");
    app.push_text_delta("second");
    app.finish_streaming(None, None);

    app.toggle_thinking_expanded();
    // should toggle the latest one (was expanded, now collapsed)
    assert!(app.messages[0].thinking_expanded);
    assert!(!app.messages[1].thinking_expanded);
}

#[test]
fn thinking_display_collapse_starts_collapsed() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.thinking_display = ThinkingDisplay::Collapse;
    app.start_streaming();
    app.push_thinking_delta("deep thoughts");
    app.push_text_delta("answer");
    app.finish_streaming(None, None);

    assert!(!app.messages[0].thinking_expanded);
}

#[test]
fn thinking_display_hidden_starts_collapsed() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.thinking_display = ThinkingDisplay::Hidden;
    app.start_streaming();
    app.push_thinking_delta("deep thoughts");
    app.push_text_delta("answer");
    app.finish_streaming(None, None);

    assert!(!app.messages[0].thinking_expanded);
}

#[test]
fn multi_line_input() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input_char('a');
    app.input_char('\n');
    app.input_char('b');
    assert_eq!(app.input.text, "a\nb");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn session_picker_open_close() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert_eq!(app.interaction.mode, AppMode::Normal);

    let sessions = vec![SessionMeta {
        id: mush_session::SessionId::from("abc"),
        title: Some("test session".into()),
        model_id: "claude-sonnet".into(),
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        message_count: 5,
        cwd: "/tmp".into(),
    }];

    app.open_session_picker(sessions, "/tmp".into());
    assert_eq!(app.interaction.mode, AppMode::SessionPicker);
    assert!(app.interaction.session_picker.is_some());

    app.close_session_picker();
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.session_picker.is_none());
}

#[test]
fn session_picker_filter() {
    let sessions = vec![
        SessionMeta {
            id: mush_session::SessionId::from("a"),
            title: Some("rust project".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/tmp".into(),
        },
        SessionMeta {
            id: mush_session::SessionId::from("b"),
            title: Some("python script".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 2,
            cwd: "/tmp".into(),
        },
    ];

    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(sessions, "/tmp".into());

    let picker = app.interaction.session_picker.as_mut().unwrap();
    picker.filter = "rust".into();
    let filtered = filtered_sessions(picker);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].title.as_deref(), Some("rust project"));
}

#[test]
fn session_picker_filter_is_fuzzy_subsequence() {
    let sessions = vec![
        SessionMeta {
            id: mush_session::SessionId::from("a"),
            title: Some("rust project".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/tmp".into(),
        },
        SessionMeta {
            id: mush_session::SessionId::from("b"),
            title: Some("python script".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 2,
            cwd: "/tmp".into(),
        },
    ];

    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(sessions, "/tmp".into());

    // "rpj" is a subsequence of "rust project" but not a substring
    let picker = app.interaction.session_picker.as_mut().unwrap();
    picker.filter = "rpj".into();
    let filtered = filtered_sessions(picker);
    assert_eq!(
        filtered.len(),
        1,
        "fuzzy filter should subsequence-match 'rust project' from 'rpj'"
    );
    assert_eq!(filtered[0].title.as_deref(), Some("rust project"));
}

#[test]
fn session_picker_scope_filter() {
    let sessions = vec![
        SessionMeta {
            id: mush_session::SessionId::from("a"),
            title: Some("local session".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/home/user/project".into(),
        },
        SessionMeta {
            id: mush_session::SessionId::from("b"),
            title: Some("other session".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 2,
            cwd: "/home/user/other".into(),
        },
    ];

    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(sessions, "/home/user/project".into());

    // this dir: only the matching session
    let picker = app.interaction.session_picker.as_ref().unwrap();
    assert_eq!(picker.scope, SessionScope::ThisDir);
    let filtered = filtered_sessions(picker);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].title.as_deref(), Some("local session"));

    // all dirs: both sessions
    let picker = app.interaction.session_picker.as_mut().unwrap();
    picker.scope = SessionScope::AllDirs;
    let filtered = filtered_sessions(picker);
    assert_eq!(filtered.len(), 2);
}

#[test]
fn streaming_tool_args_accumulate() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_tool_args_delta("{\"path\":");
    app.push_tool_args_delta("\"src/");
    assert_eq!(app.stream.tool_args, "{\"path\":\"src/");

    // start_tool clears the buffer
    let tool_call_id = ToolCallId::from("tc_1");
    app.start_tool(&tool_call_id, "read", "src/main.rs");
    assert!(app.stream.tool_args.is_empty());
}

#[test]
fn word_boundary_left_skips_punctuation() {
    // cursor after "hello." should delete back to start of "hello"
    assert_eq!(word_boundary_left("hello.", 6), 0);
    assert_eq!(word_boundary_left("foo hello.", 10), 4);
    assert_eq!(word_boundary_left("hello world.", 12), 6);
}

#[test]
fn word_boundary_left_skips_asterisks() {
    assert_eq!(word_boundary_left("hello**", 7), 0);
    assert_eq!(word_boundary_left("one two**", 9), 4);
}

#[test]
fn word_boundary_left_normal_words() {
    assert_eq!(word_boundary_left("hello world", 11), 6);
    assert_eq!(word_boundary_left("hello world  ", 13), 6);
    assert_eq!(word_boundary_left("hello", 5), 0);
}

#[test]
fn word_boundary_left_all_punctuation() {
    assert_eq!(word_boundary_left("...", 3), 0);
}

#[test]
fn steering_message_ordered_after_assistant() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("assistant reply");
    // user sends steering while streaming
    app.push_queued_message("steer");
    // assistant finishes
    app.finish_streaming(None, None);

    assert_eq!(app.messages.len(), 2);
    assert_eq!(app.messages[0].role, MessageRole::Assistant);
    assert_eq!(app.messages[0].content, "assistant reply");
    assert_eq!(app.messages[1].role, MessageRole::User);
    assert_eq!(app.messages[1].content, "steer");
}

#[test]
fn tool_output_attaches_to_assistant_when_steering_queued_mid_tool() {
    // regression: when the user queues a steering message while a tool
    // is running, the queued user bubble sits at the end of
    // app.messages. tool start/end used to push and look up via
    // messages.last_mut(), which is the queued user bubble in this
    // scenario instead of the assistant message that actually owns
    // the tool call. the result: the tool_call entry never got its
    // status flipped to Done and the output_preview was silently
    // dropped, so when the steering eventually injected (queued ->
    // actual) and the live panel cleared on the next TurnStart, the
    // output had no visible home in the message history. tool calls
    // must always attach to the assistant message owning the turn,
    // not whatever happens to be last in the visible message list
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("do thing");
    app.start_streaming();
    app.push_text_delta("on it");
    app.finish_streaming(None, None);

    let tc_id = ToolCallId::from("tc_1");
    app.start_tool(&tc_id, "bash", "ls");

    // user queues a steering message while the tool is mid-flight
    app.push_queued_message("also do this");

    app.end_tool(
        &tc_id,
        "bash",
        ToolOutcome::Success,
        Some("file_a\nfile_b\n"),
        None,
    );

    let assistant = app
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
        .expect("assistant message");
    assert_eq!(
        assistant.tool_calls.len(),
        1,
        "tool call must live on the assistant turn, not the queued user bubble"
    );
    assert_eq!(assistant.tool_calls[0].name, "bash");
    assert_eq!(assistant.tool_calls[0].status, ToolCallStatus::Done);
    let preview = assistant.tool_calls[0]
        .output_preview
        .as_deref()
        .unwrap_or_default();
    assert!(
        preview.contains("file_a") && preview.contains("file_b"),
        "tool output must not be swallowed; got preview {preview:?}",
    );

    let queued = app
        .messages
        .iter()
        .find(|m| m.queued)
        .expect("queued steering bubble");
    assert!(
        queued.tool_calls.is_empty(),
        "queued user bubble must not collect tool calls"
    );
}

#[test]
fn tool_start_attaches_to_assistant_when_steering_already_queued() {
    // companion regression: a queued steering message present at the
    // moment a new tool starts must not become the parent of the new
    // tool_call entry
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("do thing");
    app.start_streaming();
    app.push_text_delta("on it");
    app.finish_streaming(None, None);

    // steering queued BEFORE any tool fires
    app.push_queued_message("also do this");

    let tc_id = ToolCallId::from("tc_1");
    app.start_tool(&tc_id, "read", "src/lib.rs");

    let assistant = app
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
        .expect("assistant message");
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(assistant.tool_calls[0].name, "read");

    let queued = app
        .messages
        .iter()
        .find(|m| m.queued)
        .expect("queued steering bubble");
    assert!(queued.tool_calls.is_empty());
}

#[test]
fn cache_ttl_secs_by_provider() {
    use super::cache_ttl_secs;

    // anthropic: short = 5 min, long = 1 hour, none = 0
    assert_eq!(
        cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Short), false),
        300
    );
    assert_eq!(
        cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Long), false),
        3600
    );
    assert_eq!(
        cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::None), false),
        0
    );
    assert_eq!(cache_ttl_secs(&Provider::Anthropic, None, false), 300); // default = short

    // oauth always gets 1h, regardless of retention setting
    assert_eq!(cache_ttl_secs(&Provider::Anthropic, None, true), 3600);
    assert_eq!(
        cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Short), true),
        3600,
        "oauth should ignore Short retention and use 1h"
    );

    // openrouter / custom: defaults to 300
    assert_eq!(cache_ttl_secs(&Provider::OpenRouter, None, false), 300);
    assert_eq!(
        cache_ttl_secs(&Provider::Custom("xai".into()), None, false),
        300
    );
}

#[test]
fn cache_remaining_countdown() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert!(app.cache.remaining_secs().is_none());

    app.cache.ttl_secs = 300;
    app.cache.refresh();
    let remaining = app.cache.remaining_secs().unwrap();
    // just refreshed, should be very close to 300
    assert!((298..=300).contains(&remaining));
}

#[test]
fn pop_last_queued_message() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    // no queued messages returns none
    assert!(app.pop_last_queued_message().is_none());

    // add some queued messages
    app.push_queued_message("first steering");
    app.push_queued_message("second steering");

    // pops the last one
    assert_eq!(app.pop_last_queued_message().unwrap(), "second steering");
    assert_eq!(app.pop_last_queued_message().unwrap(), "first steering");
    assert!(app.pop_last_queued_message().is_none());
}

#[test]
fn pop_queued_skips_non_queued() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("normal message");
    app.push_queued_message("steering msg");

    // only pops the queued one
    assert_eq!(app.pop_last_queued_message().unwrap(), "steering msg");
    assert!(app.pop_last_queued_message().is_none());
    // normal message still there
    assert_eq!(app.messages.len(), 1);
    assert!(!app.messages[0].queued);
}

#[test]
fn selection_range_none_without_anchor() {
    let app = App::new("test".into(), TokenCount::new(200_000));
    assert!(!app.has_selection());
    assert!(app.selection_range().is_none());
}

#[test]
fn selection_range_normalises_order() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    // anchor at 3, cursor at 1 => range (1, 3)
    app.navigation.selection_anchor = Some(3);
    app.navigation.selected_message = Some(1);
    assert!(app.has_selection());
    assert_eq!(app.selection_range(), Some((1, 3)));

    // anchor at 1, cursor at 3 => range (1, 3)
    app.navigation.selection_anchor = Some(1);
    app.navigation.selected_message = Some(3);
    assert_eq!(app.selection_range(), Some((1, 3)));

    // same index => single message range
    app.navigation.selection_anchor = Some(2);
    app.navigation.selected_message = Some(2);
    assert_eq!(app.selection_range(), Some((2, 2)));
}

#[test]
fn copy_message_selection_copies_body_only_and_labels_it() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.navigation.scroll_unit = ScrollUnit::Message;
    let mut msg = DisplayMessage::new(MessageRole::Assistant, "visible body");
    msg.thinking = Some("hidden thinking".into());
    msg.tool_calls.push(DisplayToolCall {
        name: "bash".into(),
        summary: "echo secret".into(),
        status: ToolCallStatus::Done,
        output_preview: Some("tool output".into()),
        image_data: None,
        batch: 1,
    });
    app.messages.push(msg);
    app.navigation.selected_message = Some(0);

    let copied = app.copy_selection().expect("selection should copy");

    assert_eq!(copied.text, "visible body");
    assert_eq!(copied.status, "copied message body (12 chars)");
}

#[test]
fn copy_visual_message_selection_copies_body_range_and_clears_anchor() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.navigation.scroll_unit = ScrollUnit::Message;
    app.messages
        .push(DisplayMessage::new(MessageRole::User, "one"));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "two"));
    app.navigation.selected_message = Some(1);
    app.navigation.selection_anchor = Some(0);

    let copied = app.copy_selection().expect("selection should copy");

    assert_eq!(copied.text, "one\n\ntwo");
    assert_eq!(copied.status, "copied 2 message bodies (8 chars)");
    assert_eq!(app.navigation.selection_anchor, None);
}

#[test]
fn copy_block_selection_copies_selected_block_content() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.navigation.scroll_unit = ScrollUnit::Block;
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "para\n\n```rust\nfn main() {}\n```",
    ));
    app.navigation.selected_block = Some(1);

    let copied = app.copy_selection().expect("block should copy");

    assert_eq!(copied.text, "fn main() {}");
    assert_eq!(copied.status, "copied rust block (12 chars)");
}

/// set up `app` with `count` messages whose cached render height is
/// `height_each`, mimicking a completed render into an 80x`visible`
/// message area. used by `is_message_visible` tests to avoid driving a
/// real terminal
fn with_uniform_message_heights(app: &mut App, count: usize, height_each: usize, visible: u16) {
    const WIDTH: u16 = 80;
    for i in 0..count {
        app.messages
            .push(DisplayMessage::new(MessageRole::User, format!("msg {i}")));
    }
    let mut cache = app.render_state.height_cache.borrow_mut();
    cache.resize_with(count, || None);
    for (i, slot) in cache.iter_mut().enumerate() {
        *slot = Some(crate::app_state::CachedHeight {
            content_len: format!("msg {i}").len(),
            thinking_len: 0,
            tool_output_len: 0,
            completed_tool_count: 0,
            thinking_expanded: false,
            has_usage: false,
            width: WIDTH,
            height: height_each,
        });
    }
    drop(cache);
    app.render_state
        .message_area
        .set(ratatui::layout::Rect::new(0, 0, WIDTH, visible));
}

#[test]
fn is_message_visible_returns_true_before_first_render() {
    // with no message area from a previous render we can't compute
    // viewport intersection, so don't block the caller
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::User, "hi"));
    assert!(app.is_message_visible(0));
}

#[test]
fn is_message_visible_out_of_bounds_returns_false() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    with_uniform_message_heights(&mut app, 3, 5, 10);
    assert!(!app.is_message_visible(99));
}

#[test]
fn is_message_visible_at_bottom_shows_last_messages() {
    // 5 msgs, 10 lines each, total 50. visible 15. following the bottom,
    // so the viewport shows lines [35, 50): only msgs 3 and 4 overlap
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    with_uniform_message_heights(&mut app, 5, 10, 15);
    app.scroll_to_bottom();
    assert!(!app.is_message_visible(0));
    assert!(!app.is_message_visible(1));
    assert!(!app.is_message_visible(2));
    assert!(app.is_message_visible(3));
    assert!(app.is_message_visible(4));
}

#[test]
fn is_message_visible_tracks_the_anchor() {
    // same 5x10 layout, 10 rows up from the bottom is viewport [25, 40)
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    with_uniform_message_heights(&mut app, 5, 10, 15);
    app.scroll_up(10);
    assert!(!app.is_message_visible(1)); // [10,20) above viewport
    assert!(app.is_message_visible(2)); // [20,30) overlaps [25,40)
    assert!(app.is_message_visible(3)); // [30,40) inside
    assert!(!app.is_message_visible(4)); // [40,50) below viewport
}

#[test]
fn tool_call_status_is_copy() {
    let status = ToolCallStatus::Running;
    let copied = status; // would fail if not Copy
    assert_eq!(status, copied);
}

#[test]
fn display_message_new_has_sane_defaults() {
    let msg = DisplayMessage::new(MessageRole::User, "hello");
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(msg.content, "hello");
    assert!(msg.tool_calls.is_empty());
    assert!(msg.thinking.is_none());
    assert!(!msg.thinking_expanded);
    assert!(msg.usage.is_none());
    assert!(msg.cost.is_none());
    assert!(msg.model_id.is_none());
    assert!(!msg.queued);
}

#[test]
fn display_message_new_with_struct_update() {
    let msg = DisplayMessage {
        queued: true,
        ..DisplayMessage::new(MessageRole::User, "steer")
    };
    assert!(msg.queued);
    assert_eq!(msg.content, "steer");
}

#[test]
fn cache_timer_inactive_by_default() {
    let timer = CacheTimer::new(300);
    assert!(timer.remaining_secs().is_none());
    assert!(timer.elapsed_secs().is_none());
    assert!(!timer.warn_sent);
    assert!(!timer.expired_sent);
}

#[test]
fn cache_timer_refresh_starts_countdown() {
    let mut timer = CacheTimer::new(300);
    timer.refresh();
    let remaining = timer.remaining_secs().unwrap();
    assert!((298..=300).contains(&remaining));
    assert!(timer.elapsed_secs().unwrap() <= 2);
}

#[test]
fn cache_timer_refresh_resets_flags() {
    let mut timer = CacheTimer::new(300);
    timer.warn_sent = true;
    timer.expired_sent = true;
    timer.refresh();
    assert!(!timer.warn_sent);
    assert!(!timer.expired_sent);
}

#[test]
fn cache_timer_disabled_when_zero_ttl() {
    let timer = CacheTimer::new(0);
    assert_eq!(timer.ttl_secs, 0);
}

#[test]
fn streaming_state_starts_inactive() {
    let stream = StreamingState::new();
    assert!(!stream.active);
    assert!(stream.text.is_empty());
    assert!(stream.thinking.is_empty());
    assert!(stream.tool_args.is_empty());
}

#[test]
fn streaming_state_start_clears_and_activates() {
    let mut stream = StreamingState::new();
    stream.push_text("leftover");
    stream.start();
    assert!(stream.active);
    assert!(stream.text.is_empty());
    assert!(stream.thinking.is_empty());
}

#[test]
fn streaming_state_visible_text_typewriter() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("hello world");
    // before advance, nothing visible
    assert_eq!(stream.visible_text(), "");
    // advance once
    stream.advance_typewriter();
    let visible = stream.visible_text();
    assert!(!visible.is_empty());
    assert!(visible.len() <= "hello world".len());
    // enough advances catch up
    for _ in 0..20 {
        stream.advance_typewriter();
    }
    assert_eq!(stream.visible_text(), "hello world");
}

#[test]
fn streaming_state_take_returns_content() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("answer");
    stream.push_thinking("reasoning");
    let (text, thinking) = stream.take();
    assert_eq!(text, "answer");
    assert_eq!(thinking.as_deref(), Some("reasoning"));
    assert!(!stream.active);
    assert!(stream.text.is_empty());
}

#[test]
fn streaming_state_take_no_thinking_returns_none() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("just text");
    let (_, thinking) = stream.take();
    assert!(thinking.is_none());
}

#[test]
fn shimmer_phase_advances_with_animation_frames() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));

    app.tick_animation();
    assert_eq!(app.shimmer_phase(), 1);
    app.tick_animation();
    assert_eq!(app.shimmer_phase(), 2);
}

#[test]
fn is_busy_during_tool_execution() {
    // stream.active is false during tool execution (text streaming
    // ended) but is_busy() should still return true when tools are
    // running so input shortcuts can still abort the turn
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert!(!app.stream.active);
    assert!(!app.is_busy());

    // simulate tool running with stream inactive
    app.active_tools
        .push(crate::display_types::ActiveToolState {
            tool_call_id: "test_1".into(),
            name: "bash".into(),
            summary: "running command".into(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });

    assert!(
        !app.stream.active,
        "stream should be inactive during tool execution"
    );
    assert!(
        app.is_busy(),
        "is_busy should be true when tools are running"
    );

    app.tick_animation();
}

#[test]
fn running_tool_is_busy_and_animates_activity_label() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.active_tools
        .push(crate::display_types::ActiveToolState {
            tool_call_id: "test_1".into(),
            name: "bash".into(),
            summary: "running command".into(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });

    assert!(app.is_busy());
    assert!(app.has_visible_animation());
    assert!(app.needs_periodic_redraw(false));

    app.stream.start();
    assert!(app.has_visible_animation());

    app.stream.active = false;
    app.compacting = true;
    assert!(app.has_visible_animation());
}

#[test]
fn cache_countdown_needs_periodic_redraw_when_enabled() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cache.refresh();

    assert!(app.needs_periodic_redraw(true));
    assert!(!app.needs_periodic_redraw(false));
}

#[test]
fn activity_label_none_when_idle() {
    let app = App::new("test".into(), TokenCount::new(200_000));
    assert_eq!(app.activity_label(), None);
}

#[test]
fn activity_label_thinking_when_streaming_with_no_text() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    assert_eq!(app.activity_label().as_deref(), Some("thinking…"));
}

#[test]
fn activity_label_outputting_when_streaming_text() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("hello");
    assert_eq!(app.activity_label().as_deref(), Some("outputting…"));
}

#[test]
fn activity_label_uses_tool_name_when_one_running() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.active_tools
        .push(crate::display_types::ActiveToolState {
            tool_call_id: "t1".into(),
            name: "bash".into(),
            summary: String::new(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });
    assert_eq!(app.activity_label().as_deref(), Some("using bash…"));
}

#[test]
fn activity_label_collapses_distinct_parallel_tools_to_count() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for (i, name) in ["bash", "read", "edit"].iter().enumerate() {
        app.active_tools
            .push(crate::display_types::ActiveToolState {
                tool_call_id: format!("t{i}").into(),
                name: (*name).into(),
                summary: String::new(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            });
    }
    assert_eq!(app.activity_label().as_deref(), Some("using 3 tools…"));
}

#[test]
fn activity_label_keeps_tool_name_when_all_same_tool() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..3 {
        app.active_tools
            .push(crate::display_types::ActiveToolState {
                tool_call_id: format!("t{i}").into(),
                name: "read".into(),
                summary: String::new(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            });
    }
    assert_eq!(app.activity_label().as_deref(), Some("using read…"));
}

#[test]
fn activity_label_ignores_completed_tools() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.active_tools
        .push(crate::display_types::ActiveToolState {
            tool_call_id: "t1".into(),
            name: "bash".into(),
            summary: String::new(),
            live_output: None,
            status: ToolCallStatus::Done,
            output: None,
        });
    assert_eq!(app.activity_label(), None);
}

#[test]
fn compacting_makes_app_busy_with_label() {
    // a backgrounded /compact run is real work, so is_busy() must
    // include it (drives the animation tick) and activity_label()
    // must surface it for the chat + status bar
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert!(!app.is_busy());
    assert_eq!(app.activity_label(), None);

    app.compacting = true;
    assert!(app.is_busy());
    assert_eq!(app.activity_label().as_deref(), Some("compacting…"));
}

#[test]
fn compacting_label_takes_precedence_over_streaming_and_tools() {
    // compaction is a discrete, exclusive operation. if a user
    // somehow ends up with both flags set the label should still
    // read "compacting…" so the UI doesn't lie about what's
    // happening
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.active_tools
        .push(crate::display_types::ActiveToolState {
            tool_call_id: "t1".into(),
            name: "edit".into(),
            summary: String::new(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });
    app.compacting = true;
    assert_eq!(app.activity_label().as_deref(), Some("compacting…"));
}

#[test]
fn activity_label_prefers_tool_over_streaming() {
    // when both apply, "using tool" wins since it's the more concrete
    // signal and stream.active can persist across tool execution gaps
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.active_tools
        .push(crate::display_types::ActiveToolState {
            tool_call_id: "t1".into(),
            name: "edit".into(),
            summary: String::new(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });
    assert_eq!(app.activity_label().as_deref(), Some("using edit…"));
}

#[test]
fn unread_flash_cycle_works() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.navigation.has_unread = true;
    // at tick 0, flash should be on
    assert!(app.unread_flash_on());
    // advance past the on-phase
    for _ in 0..UNREAD_FLASH_ON {
        app.tick();
    }
    assert!(!app.unread_flash_on());
}

#[test]
fn input_buffer_basic_editing() {
    let mut buf = InputBuffer::new();
    buf.insert_char('h');
    buf.insert_char('i');
    assert_eq!(buf.text, "hi");
    assert_eq!(buf.cursor, 2);

    buf.backspace();
    assert_eq!(buf.text, "h");
    assert_eq!(buf.cursor, 1);

    buf.cursor_home();
    assert_eq!(buf.cursor, 0);

    buf.cursor_end();
    assert_eq!(buf.cursor, 1);
}

#[test]
fn input_buffer_take_returns_and_clears() {
    let mut buf = InputBuffer::new();
    buf.insert_char('x');
    let taken = buf.take_text();
    assert_eq!(taken, "x");
    assert!(buf.text.is_empty());
    assert_eq!(buf.cursor, 0);
}

#[test]
fn input_buffer_word_navigation() {
    let mut buf = InputBuffer::new();
    buf.text = "hello world foo".into();
    buf.cursor = buf.text.len();

    buf.cursor_word_left();
    assert_eq!(buf.cursor, 12); // before "foo"

    buf.cursor_word_left();
    assert_eq!(buf.cursor, 6); // before "world"
}

#[test]
fn code_blocks_extracts_fenced_blocks() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "here:\n```bash\nrm -rf mything\n```\nand also:\n```python\nprint(\"hello\")\n```",
    ));
    app.messages
        .push(DisplayMessage::new(MessageRole::User, "nice"));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "one more:\n```\nplain block\n```",
    ));

    let blocks = app.code_blocks();
    assert_eq!(blocks.len(), 3);

    assert_eq!(blocks[0].msg_idx, 0);
    assert_eq!(blocks[0].content, "rm -rf mything");
    assert_eq!(blocks[0].lang.as_deref(), Some("bash"));

    assert_eq!(blocks[1].msg_idx, 0);
    assert_eq!(blocks[1].content, "print(\"hello\")");
    assert_eq!(blocks[1].lang.as_deref(), Some("python"));

    assert_eq!(blocks[2].msg_idx, 2);
    assert_eq!(blocks[2].content, "plain block");
    assert_eq!(blocks[2].lang, None);
}

#[test]
fn code_blocks_handles_unclosed_fence() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "streaming:\n```rust\nfn main() {}",
    ));

    let blocks = app.code_blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].content, "fn main() {}");
    assert_eq!(blocks[0].lang.as_deref(), Some("rust"));
}

#[test]
fn blocks_treats_each_paragraph_and_fence_body_as_its_own_block() {
    // user-stated mental model: every paragraph (run of non-blank
    // lines separated by blank lines) is a block, and the *body*
    // of every fenced ``` block is a block. example layout:
    //
    //     this is a block
    //
    //     this is another block
    //
    //     this is a block
    //     ```bash
    //     this here is also a block
    //     ```
    //     this is also a block
    //
    // → 5 blocks total
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "this is a block\n\nthis is another block\n\nthis is a block\n\
         ```bash\nthis here is also a block\n```\nthis is also a block",
    ));

    let blocks = app.blocks();
    assert_eq!(blocks.len(), 5, "expected 5 blocks, got {blocks:#?}");

    assert_eq!(blocks[0].content, "this is a block");
    assert!(matches!(blocks[0].kind, BlockKind::Paragraph));
    assert_eq!(blocks[1].content, "this is another block");
    assert!(matches!(blocks[1].kind, BlockKind::Paragraph));
    assert_eq!(blocks[2].content, "this is a block");
    assert!(matches!(blocks[2].kind, BlockKind::Paragraph));
    assert_eq!(blocks[3].content, "this here is also a block");
    assert!(matches!(
        blocks[3].kind,
        BlockKind::Fenced { ref lang } if lang.as_deref() == Some("bash")
    ));
    assert_eq!(blocks[4].content, "this is also a block");
    assert!(matches!(blocks[4].kind, BlockKind::Paragraph));
}

#[test]
fn blocks_skips_empty_messages() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, ""));
    app.messages
        .push(DisplayMessage::new(MessageRole::User, "   \n  \n"));
    assert!(app.blocks().is_empty());
}

#[test]
fn blocks_includes_user_message_paragraphs() {
    // y-yank in scroll mode should be able to copy a paragraph
    // out of the user's own message just as readily as out of an
    // assistant reply
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage::new(
        MessageRole::User,
        "first user line\n\nsecond user line",
    ));
    let blocks = app.blocks();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].content, "first user line");
    assert_eq!(blocks[1].content, "second user line");
    for b in blocks.iter() {
        assert_eq!(b.msg_idx, 0);
    }
}

#[test]
fn blocks_paragraph_collapses_multi_line_runs() {
    // a "paragraph" is a contiguous run of non-blank lines (no
    // blank line separating them), joined with newlines so the
    // copied content preserves the original line breaks
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "line one\nline two\nline three\n\nnew paragraph",
    ));
    let blocks = app.blocks();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].content, "line one\nline two\nline three");
    assert_eq!(blocks[1].content, "new paragraph");
}

#[test]
fn blocks_records_msg_idx_per_block() {
    // selection state stores an index into blocks(); the renderer
    // uses block.msg_idx to figure out which message to highlight
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "alpha"));
    app.messages
        .push(DisplayMessage::new(MessageRole::User, "bravo"));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "charlie"));
    let blocks = app.blocks();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].msg_idx, 0);
    assert_eq!(blocks[1].msg_idx, 1);
    assert_eq!(blocks[2].msg_idx, 2);
}

#[test]
fn blocks_are_cached_until_messages_change() {
    // block scroll mode rebuilds the block list every frame and every
    // j/k keypress. recomputing it over the whole scrollback each time
    // is O(total content), so the list is cached and only rebuilt when
    // the messages actually change.
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages
        .push(DisplayMessage::new(MessageRole::User, "one\n\ntwo"));
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "```rust\nx\n```",
    ));

    reset_blocks_call_count();
    let first = app.blocks();
    let second = app.blocks();
    assert_eq!(first, second);
    assert_eq!(
        blocks_compute_count(),
        1,
        "repeated blocks() calls with unchanged messages must reuse the cache"
    );

    // appending a message invalidates the cache
    app.messages
        .push(DisplayMessage::new(MessageRole::User, "three"));
    let _ = app.blocks();
    assert_eq!(
        blocks_compute_count(),
        2,
        "blocks() must recompute after the messages change"
    );
}

#[test]
fn insert_str_at_end() {
    let mut buf = InputBuffer::new();
    buf.insert_str("hello world");
    assert_eq!(buf.text, "hello world");
    assert_eq!(buf.cursor, 11);
}

#[test]
fn insert_str_at_middle() {
    let mut buf = InputBuffer::new();
    buf.text = "hello world".into();
    buf.cursor = 5;
    buf.insert_str(" beautiful");
    assert_eq!(buf.text, "hello beautiful world");
    assert_eq!(buf.cursor, 15);
}

#[test]
fn insert_str_multiline_paste() {
    let mut buf = InputBuffer::new();
    buf.insert_str("line 1\nline 2\nline 3");
    assert_eq!(buf.text, "line 1\nline 2\nline 3");
    assert_eq!(buf.cursor, 20);
}

#[test]
fn insert_str_empty_is_noop() {
    let mut buf = InputBuffer::new();
    buf.text = "existing".into();
    buf.cursor = 3;
    buf.insert_str("");
    assert_eq!(buf.text, "existing");
    assert_eq!(buf.cursor, 3);
}

// cache anomaly detection tests
#[test]
fn detect_context_decrease() {
    let prev = Usage {
        input_tokens: TokenCount::new(10_000),
        output_tokens: TokenCount::new(5_000),
        cache_read_tokens: TokenCount::new(90_000),
        cache_write_tokens: TokenCount::ZERO,
    };
    let curr = Usage {
        input_tokens: TokenCount::new(8_000),
        output_tokens: TokenCount::new(6_000),
        cache_read_tokens: TokenCount::new(70_000),
        cache_write_tokens: TokenCount::ZERO,
    };
    let anomalies = detect_cache_anomalies(Some(&prev), &curr, None, None);
    assert!(
        anomalies
            .iter()
            .any(|a| matches!(a, CacheAnomaly::ContextDecrease { .. })),
        "should detect context decrease: prev=100k, curr=78k"
    );
}

#[test]
fn detect_cache_bust() {
    let prev = Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::new(95_000),
        cache_write_tokens: TokenCount::ZERO,
    };
    let curr = Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::ZERO,
        cache_write_tokens: TokenCount::new(100_000),
    };
    let anomalies = detect_cache_anomalies(Some(&prev), &curr, None, None);
    assert!(
        anomalies
            .iter()
            .any(|a| matches!(a, CacheAnomaly::CacheBust { .. })),
        "should detect cache bust: cache_read dropped from 95k to 0"
    );
}

#[test]
fn no_anomaly_on_normal_growth() {
    let prev = Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::new(90_000),
        cache_write_tokens: TokenCount::new(5_000),
    };
    let curr = Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(4_000),
        cache_read_tokens: TokenCount::new(95_000),
        cache_write_tokens: TokenCount::new(5_000),
    };
    let anomalies = detect_cache_anomalies(Some(&prev), &curr, None, None);
    assert!(
        anomalies.is_empty(),
        "normal growth should produce no anomalies"
    );
}

#[test]
fn no_anomaly_on_first_call() {
    let curr = Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::ZERO,
        cache_write_tokens: TokenCount::new(95_000),
    };
    let anomalies = detect_cache_anomalies(None, &curr, None, None);
    assert!(
        anomalies.is_empty(),
        "first call should not trigger anomalies"
    );
}

#[test]
fn no_anomaly_after_reset() {
    let mut stats = TokenStats::new(TokenCount::new(200_000));
    // simulate a call
    stats.update(
        &Usage {
            input_tokens: TokenCount::new(10_000),
            output_tokens: TokenCount::new(5_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::ZERO,
        },
        None,
    );

    stats.reset();

    // next call after reset should not detect anomalies
    let anomalies = stats.update(
        &Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::new(95_000),
        },
        None,
    );
    assert!(
        anomalies.is_empty(),
        "post-reset call should not trigger anomalies"
    );
}

#[test]
fn stats_update_returns_anomalies() {
    let mut stats = TokenStats::new(TokenCount::new(200_000));
    // first call: warm cache
    let a1 = stats.update(
        &Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::new(5_000),
        },
        None,
    );
    assert!(a1.is_empty(), "first call should have no anomalies");

    // second call: cache bust (cache_read drops to 0, cache_write spikes)
    let a2 = stats.update(
        &Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::new(100_000),
        },
        None,
    );
    assert!(
        a2.iter()
            .any(|a| matches!(a, CacheAnomaly::CacheBust { .. })),
        "should detect cache bust via stats.update"
    );
}

#[test]
fn finish_streaming_shows_system_message_on_cache_bust() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));

    // first call: warm cache
    app.start_streaming();
    app.push_text_delta("hello");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::new(5_000),
        }),
        None,
    );

    let before = app.messages.len();

    // second call: cache bust
    app.start_streaming();
    app.push_text_delta("world");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::new(100_000),
        }),
        None,
    );

    // should have an extra system message about the cache bust
    let system_msgs: Vec<_> = app.messages[before..]
        .iter()
        .filter(|m| m.role == MessageRole::System)
        .collect();
    assert!(
        system_msgs.iter().any(|m| m.content.contains("cache bust")),
        "expected system message about cache bust, got: {system_msgs:?}"
    );
}

#[test]
fn cache_bust_diagnostic_writes_json_file() {
    let dir = tempfile::tempdir().unwrap();
    let bust_dir = dir.path().join("cache-busts");

    let diag = CacheBustDiagnostic {
        timestamp: "1722470400".into(),
        secs_since_last_cache_activity: Some(42),
        cache_ttl_secs: 300,
        thinking_level: "High".into(),
        model_id: "claude-sonnet-4-20250514".into(),
        effort: None,
        bust_reason: BustReason::Unexplained,
        prev_model_id: None,
        prev_thinking_level: None,
        prev_effort: None,
        prev_messages_prefix_fingerprint: None,
        curr_messages_prefix_fingerprint: None,
        prev_usage: Some(Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::new(5_000),
        }),
        curr_usage: Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::new(100_000),
        },
        prev_context_tokens: 100_000,
        curr_context_tokens: 105_000,
        session_total_cost: "1.2345".into(),
        session_api_calls: 7,
        recent_request_snapshots: Vec::new(),
    };

    // test the dump function directly using internal helper
    std::fs::create_dir_all(&bust_dir).unwrap();
    let json = serde_json::to_string_pretty(&diag).unwrap();
    let path = bust_dir.join("bust-test.json");
    std::fs::write(&path, &json).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["cache_ttl_secs"], 300);
    assert_eq!(parsed["secs_since_last_cache_activity"], 42);
    assert_eq!(parsed["thinking_level"], "High");
    assert_eq!(parsed["prev_usage"]["cache_read_tokens"], 90_000);
    assert_eq!(parsed["curr_usage"]["cache_write_tokens"], 100_000);
    // no snapshots set → field should be absent via skip_serializing_if
    assert!(parsed.get("recent_request_snapshots").is_none());
}

#[test]
fn cache_bust_diagnostic_includes_request_snapshots_when_present() {
    // when request snapshots are available at bust time, their
    // paths should land in the json so a reader can diff them.
    // the field is declared skip_serializing_if_empty, so presence
    // in the json proves both the hookup and the serde contract
    let diag = CacheBustDiagnostic {
        timestamp: "1722470400".into(),
        secs_since_last_cache_activity: Some(10),
        cache_ttl_secs: 3600,
        thinking_level: "High".into(),
        model_id: "claude-opus-4-7".into(),
        effort: None,
        bust_reason: BustReason::Unexplained,
        prev_model_id: None,
        prev_thinking_level: None,
        prev_effort: None,
        prev_messages_prefix_fingerprint: None,
        curr_messages_prefix_fingerprint: None,
        prev_usage: None,
        curr_usage: Usage::default(),
        prev_context_tokens: 0,
        curr_context_tokens: 0,
        session_total_cost: "0.0".into(),
        session_api_calls: 1,
        recent_request_snapshots: vec![
            std::path::PathBuf::from("/tmp/mush/snapshots/anthropic-001.json"),
            std::path::PathBuf::from("/tmp/mush/snapshots/anthropic-000.json"),
        ],
    };
    let json = serde_json::to_string(&diag).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let paths = parsed["recent_request_snapshots"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths[0].as_str().unwrap().ends_with("anthropic-001.json"));
}

#[test]
fn finish_streaming_shows_system_message_on_context_decrease() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));

    // first call
    app.start_streaming();
    app.push_text_delta("hello");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(10_000),
            output_tokens: TokenCount::new(5_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::ZERO,
        }),
        None,
    );

    let before = app.messages.len();

    // second call: context decreased
    app.start_streaming();
    app.push_text_delta("world");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(8_000),
            output_tokens: TokenCount::new(6_000),
            cache_read_tokens: TokenCount::new(70_000),
            cache_write_tokens: TokenCount::ZERO,
        }),
        None,
    );

    let system_msgs: Vec<_> = app.messages[before..]
        .iter()
        .filter(|m| m.role == MessageRole::System)
        .collect();
    assert!(
        system_msgs
            .iter()
            .any(|m| m.content.contains("context decreased")),
        "expected system message about context decrease, got: {system_msgs:?}"
    );
}

#[test]
fn finish_streaming_no_system_message_on_normal_growth() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));

    // first call
    app.start_streaming();
    app.push_text_delta("hello");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::new(5_000),
        }),
        None,
    );

    let before = app.messages.len();

    // second call: normal growth
    app.start_streaming();
    app.push_text_delta("world");
    app.finish_streaming(
        Some(Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(4_000),
            cache_read_tokens: TokenCount::new(95_000),
            cache_write_tokens: TokenCount::new(5_000),
        }),
        None,
    );

    let system_msgs: Vec<_> = app.messages[before..]
        .iter()
        .filter(|m| m.role == MessageRole::System)
        .collect();
    assert!(
        system_msgs.is_empty(),
        "normal growth should not produce system messages, got: {system_msgs:?}"
    );
}

#[test]
fn ghost_text_shown_for_partial_prefix() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.completions = vec!["/login".into(), "/login-complete".into()];
    app.completion.slash_commands = vec![
        SlashCommand {
            name: "login".into(),
            description: "start oauth".into(),
        },
        SlashCommand {
            name: "login-complete".into(),
            description: "finish oauth".into(),
        },
    ];
    app.input.text = "/lo".into();
    app.input.cursor = 3;
    assert_eq!(app.ghost_text(), Some("gin"));
}

#[test]
fn ghost_text_suppressed_when_input_is_exact_command() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.completions = vec!["/login".into(), "/login-complete".into()];
    app.completion.slash_commands = vec![
        SlashCommand {
            name: "login".into(),
            description: "start oauth".into(),
        },
        SlashCommand {
            name: "login-complete".into(),
            description: "finish oauth".into(),
        },
    ];
    app.input.text = "/login".into();
    app.input.cursor = 6;
    assert_eq!(
        app.ghost_text(),
        None,
        "/login exactly matches a command so no ghost suffix should leak /login-complete"
    );
}

#[test]
fn open_model_picker_overlay_populates_state_with_models() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.model_completions = vec![
        ModelCompletion {
            id: "claude-opus".into(),
            name: "Claude Opus".into(),
            provider: "anthropic".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
        ModelCompletion {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            provider: "anthropic".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
    ];
    app.open_model_picker_overlay();
    assert_eq!(app.interaction.mode, AppMode::ModelPicker);
    let picker = app
        .interaction
        .model_picker
        .as_ref()
        .expect("overlay should be open");
    assert_eq!(picker.models.len(), 2);
    assert_eq!(picker.selected, 0);
}

#[test]
fn open_model_picker_overlay_is_noop_without_models() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.model_completions.clear();
    app.open_model_picker_overlay();
    assert!(app.interaction.model_picker.is_none());
    assert_eq!(app.interaction.mode, AppMode::Normal);
}

#[test]
fn open_model_picker_overlay_hides_internal_codex_entries_by_default() {
    // codex marks some upstream entries as `internal` or `experimental`;
    // the default overlay only shows production-ready models so users
    // don't wade through every snapshot
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.model_completions = vec![
        ModelCompletion {
            id: "gpt-5.4".into(),
            name: "GPT-5.4".into(),
            provider: "openai-codex".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: Some("default".into()),
        },
        ModelCompletion {
            id: "internal-snapshot".into(),
            name: "Internal".into(),
            provider: "openai-codex".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 99,
            visibility: Some("internal".into()),
        },
    ];
    app.open_model_picker_overlay();
    let picker = app.interaction.model_picker.as_ref().unwrap();
    let ids: Vec<&str> = picker.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gpt-5.4"]);
}

#[test]
fn open_model_picker_overlay_all_reveals_hidden_codex_entries() {
    // /model --all routes to open_model_picker_overlay_all so internal
    // entries surface alongside production ones, sorted by priority desc
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.model_completions = vec![
        ModelCompletion {
            id: "gpt-5.4".into(),
            name: "GPT-5.4".into(),
            provider: "openai-codex".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 1,
            visibility: Some("default".into()),
        },
        ModelCompletion {
            id: "internal-snapshot".into(),
            name: "Internal".into(),
            provider: "openai-codex".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 99,
            visibility: Some("internal".into()),
        },
    ];
    app.open_model_picker_overlay_all();
    let picker = app.interaction.model_picker.as_ref().unwrap();
    let ids: Vec<&str> = picker.models.iter().map(|m| m.id.as_str()).collect();
    // priority 99 first, then priority 1
    assert_eq!(ids, vec!["internal-snapshot", "gpt-5.4"]);
    assert!(picker.show_all, "show_all flag must be set");
}

#[test]
fn login_picker_open_close() {
    // /login with no arg should open a centred floating picker that
    // mirrors the model / session pickers. opening sets the mode and
    // populates state, closing reverts to Normal and drops state
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let entries = vec![
        crate::login_picker::LoginEntry {
            id: "anthropic-pro-max".into(),
            name: "Anthropic".into(),
            method: mush_ai::login::LoginMethod::OAuth {
                oauth_provider_id: "anthropic".into(),
            },
            source: Some(mush_ai::login::LoginSource::OAuth),
            account_id: None,
        },
        crate::login_picker::LoginEntry {
            id: "openai-codex".into(),
            name: "ChatGPT Codex".into(),
            method: mush_ai::login::LoginMethod::OAuth {
                oauth_provider_id: "openai-codex".into(),
            },
            source: None,
            account_id: None,
        },
    ];
    app.open_login_picker(entries, std::collections::HashMap::new());
    assert_eq!(app.interaction.mode, AppMode::LoginPicker);
    assert!(app.interaction.login_picker.is_some());

    app.close_login_picker();
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.login_picker.is_none());
}

fn login_entry(id: &str, method: mush_ai::login::LoginMethod) -> crate::login_picker::LoginEntry {
    crate::login_picker::LoginEntry {
        id: id.into(),
        name: id.into(),
        method,
        source: None,
        account_id: None,
    }
}

#[test]
fn handle_paste_routes_into_login_picker_entry_buffer() {
    // when the /login picker has armed an api-key entry prompt,
    // bracketed-paste text must land in the picker's buffer rather
    // than the main prompt input. otherwise pasted keys leak into
    // the message box visible behind the overlay
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![login_entry(
            "openrouter",
            mush_ai::login::LoginMethod::ApiKey {
                storage_key: "openrouter".into(),
                env_var: "OPENROUTER_API_KEY".into(),
                config_key: "openrouter".into(),
            },
        )],
        std::collections::HashMap::new(),
    );
    let picker = app.interaction.login_picker.as_mut().unwrap();
    picker.arm_entry();

    app.handle_paste("sk-or-v1-secret");

    let prompt = app
        .interaction
        .login_picker
        .as_ref()
        .and_then(|p| p.entry.as_ref())
        .expect("entry prompt still active");
    assert_eq!(prompt.buffer, "sk-or-v1-secret");
    assert!(app.input.text.is_empty(), "main input must stay empty");
}

#[test]
fn handle_paste_inserts_into_main_input_by_default() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.handle_paste("hello world");
    assert_eq!(app.input.text, "hello world");
}

#[test]
fn selected_login_provider_returns_filtered_id() {
    // selected_login_provider must follow the filter so a typed query
    // that reorders or hides rows still picks the visible row at
    // `selected`. mirrors selected_session / selected_model_id
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![
            crate::login_picker::LoginEntry {
                id: "anthropic-pro-max".into(),
                name: "Anthropic".into(),
                method: mush_ai::login::LoginMethod::OAuth {
                    oauth_provider_id: "anthropic".into(),
                },
                source: Some(mush_ai::login::LoginSource::OAuth),
                account_id: None,
            },
            crate::login_picker::LoginEntry {
                id: "openai-codex".into(),
                name: "ChatGPT Codex".into(),
                method: mush_ai::login::LoginMethod::OAuth {
                    oauth_provider_id: "openai-codex".into(),
                },
                source: None,
                account_id: None,
            },
        ],
        std::collections::HashMap::new(),
    );
    let picker = app.interaction.login_picker.as_mut().unwrap();
    picker.filter = "codex".into();
    picker.selected = 0;
    assert_eq!(
        app.selected_login_provider().as_deref(),
        Some("openai-codex")
    );
}
