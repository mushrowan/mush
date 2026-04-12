use mush_tui::app_event::{AppEvent, AppMode};
use mush_tui::slash::SlashAction;

#[test]
fn app_event_module_exposes_event_and_mode_types() {
    let event = AppEvent::SlashCommand {
        action: SlashAction::New,
    };
    assert!(matches!(event, AppEvent::SlashCommand { .. }));
    assert_eq!(AppMode::Normal, AppMode::Normal);
    assert_ne!(AppMode::Normal, AppMode::Search);
}
