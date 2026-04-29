use mush_tui::slash_menu::{SlashCommand, SlashMenuState};

#[test]
fn slash_menu_module_exposes_completion_state() {
    let commands = vec![SlashCommand {
        name: "help".into(),
        description: "show help".into(),
    }];
    let menu = SlashMenuState::for_commands(commands);
    assert_eq!(menu.matches[0].name, "help");
    assert_eq!(menu.selected, 0);
}
