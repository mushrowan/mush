//! verify slash module split exposes parsing and execution from sub-modules

use mush_tui::slash::commands::handle;
use mush_tui::slash::compaction::fork_and_compact;
use mush_tui::slash::{SlashAction, SlashParseError, parse};

#[test]
fn parse_types_accessible_from_slash_root() {
    let action = parse("/help").unwrap();
    assert_eq!(action, SlashAction::Help);

    let err = parse("no-slash");
    assert!(matches!(err, Err(SlashParseError::MissingPrefix)));
}

#[test]
fn handle_accessible_from_commands_submodule() {
    // just verify the function signature is importable
    let _: fn(
        &mut mush_tui::app::App,
        &mut mush_session::ConversationState,
        &mut mush_tui::TuiConfig,
        &std::collections::HashMap<String, mush_ai::types::ThinkingLevel>,
        &SlashAction,
    ) -> Option<String> = handle;
}

#[test]
fn fork_and_compact_accessible_from_compaction_submodule() {
    // verify the async function is importable
    let _ = fork_and_compact;
}
