use mush_tui::slash_menu::{ModelCompletion, SlashCommand, SlashMenuState};

#[test]
fn slash_menu_module_exposes_completion_state() {
    let commands = vec![SlashCommand {
        name: "help".into(),
        description: "show help".into(),
    }];
    let command_menu = SlashMenuState::for_commands(commands);
    assert!(!command_menu.model_mode);
    assert_eq!(command_menu.matches[0].name, "help");
    assert_eq!(command_menu.selected, 0);

    let models = vec![ModelCompletion {
        id: "claude-sonnet".into(),
        name: "Claude Sonnet".into(),
        provider: "anthropic".into(),
        stale: false,
        description: None,
        speed_tiers: Vec::new(),
    }];
    let model_menu = SlashMenuState::for_models(models);
    assert!(model_menu.model_mode);
    assert_eq!(model_menu.model_matches[0].id, "claude-sonnet");
    assert_eq!(model_menu.selected, 0);
}
