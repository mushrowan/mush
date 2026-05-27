//! verify slash module split exposes parsing and execution from sub-modules

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
    // verify the function is importable from the submodule
    use mush_tui::slash::commands::handle;
    let _ = handle as *const ();
}

#[test]
fn run_compaction_accessible_from_compaction_submodule() {
    use mush_tui::slash::compaction::run_compaction;
    let _ = run_compaction as *const ();
}
