use mush_tui::slash::{SlashAction, SlashParseError, parse};

#[test]
fn parse_types_accessible_from_slash_root() {
    let action = parse("/help").unwrap();
    assert_eq!(action, SlashAction::Help);

    let err = parse("no-slash");
    assert!(matches!(err, Err(SlashParseError::MissingPrefix)));
}
