//! slash completion state and filtering helpers

/// slash command menu item
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

/// model completion menu item
#[derive(Debug, Clone)]
pub struct ModelCompletion {
    pub id: String,
    pub name: String,
}

/// state for the slash command completion menu
#[derive(Debug, Clone)]
pub struct SlashMenuState {
    /// filtered commands matching current input
    pub matches: Vec<SlashCommand>,
    /// filtered models matching current /model query
    pub model_matches: Vec<ModelCompletion>,
    /// whether this menu is showing models
    pub model_mode: bool,
    /// which match is selected
    pub selected: usize,
}

impl SlashMenuState {
    #[must_use]
    pub fn for_commands(matches: Vec<SlashCommand>) -> Self {
        Self {
            matches,
            model_matches: Vec::new(),
            model_mode: false,
            selected: 0,
        }
    }

    #[must_use]
    pub fn for_models(model_matches: Vec<ModelCompletion>) -> Self {
        Self {
            matches: Vec::new(),
            model_matches,
            model_mode: true,
            selected: 0,
        }
    }
}

/// tracks an in-progress tab completion cycle
#[derive(Debug, Clone)]
pub(crate) struct TabState {
    /// matching candidates
    pub(crate) matches: Vec<String>,
    /// which match we're showing (cycles on repeated tab)
    pub(crate) index: usize,
}

#[must_use]
pub(crate) fn filter_command_matches(commands: &[SlashCommand], prefix: &str) -> Vec<SlashCommand> {
    commands
        .iter()
        .filter(|cmd| {
            let full = format!("/{}", cmd.name);
            full.starts_with(prefix)
        })
        .cloned()
        .collect()
}

#[must_use]
pub(crate) fn filter_model_matches(
    model_completions: &[ModelCompletion],
    prefix: &str,
) -> Vec<ModelCompletion> {
    let query = prefix.to_lowercase();
    model_completions
        .iter()
        .filter(|m| m.id.starts_with(prefix) || m.name.to_lowercase().contains(&query))
        .cloned()
        .collect()
}
