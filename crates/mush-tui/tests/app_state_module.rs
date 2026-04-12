use mush_tui::app::AppMode;
use mush_tui::app_state::{CompletionState, InteractionState, NavigationState, RenderState};

#[test]
fn app_state_module_exposes_grouped_substates() {
    let completion = CompletionState::default();
    assert!(completion.completions.is_empty());
    assert!(completion.slash_commands.is_empty());

    let interaction = InteractionState::default();
    assert_eq!(interaction.mode, AppMode::Normal);
    assert!(interaction.search.matches.is_empty());

    let navigation = NavigationState::default();
    assert!(!navigation.has_unread);
    assert!(navigation.selected_message.is_none());

    let render = RenderState::new();
    assert!(render.message_row_ranges.borrow().is_empty());
    assert!(render.markdown_cache.borrow().is_empty());
}
