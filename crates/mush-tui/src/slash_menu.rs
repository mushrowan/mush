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
    /// the model is in the discovery cache but absent from the latest
    /// fetch for its provider — likely deprecated. picker renders a
    /// `[stale]` marker so the user can spot dropped entries.
    #[allow(dead_code)] // populated by runtime; consumed by the picker widget
    pub stale: bool,
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
    /// effective favourites at open time. model_mode renders a ★ marker for
    /// ids listed here. empty vec = no favourites
    pub favourite_models: Vec<String>,
    /// whether imperative add/remove (ctrl+f in the picker) should be
    /// refused because the user declared favourites in config.toml
    pub favourites_locked: bool,
    /// toast shown at the bottom of the popup (e.g. locked notice). cleared
    /// on next keystroke
    pub toast: Option<String>,
}

impl SlashMenuState {
    #[must_use]
    pub fn for_commands(matches: Vec<SlashCommand>) -> Self {
        Self {
            matches,
            model_matches: Vec::new(),
            model_mode: false,
            selected: 0,
            favourite_models: Vec::new(),
            favourites_locked: false,
            toast: None,
        }
    }

    #[must_use]
    pub fn for_models(model_matches: Vec<ModelCompletion>) -> Self {
        Self::for_models_with_favourites(model_matches, Vec::new(), false)
    }

    /// construct a model-mode menu carrying the effective favourites list so
    /// the picker can render ★ markers and reject imperative edits when
    /// favourites are locked by config
    #[must_use]
    pub fn for_models_with_favourites(
        model_matches: Vec<ModelCompletion>,
        favourite_models: Vec<String>,
        favourites_locked: bool,
    ) -> Self {
        Self {
            matches: Vec::new(),
            model_matches,
            model_mode: true,
            selected: 0,
            favourite_models,
            favourites_locked,
            toast: None,
        }
    }

    /// whether `model_id` is in the effective favourites list
    #[must_use]
    pub fn is_favourite(&self, model_id: &str) -> bool {
        self.favourite_models.iter().any(|f| f == model_id)
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
    use crate::fuzzy::FuzzyFilter;
    if prefix.is_empty() {
        return model_completions.to_vec();
    }
    // match against the id + name concatenation so either field can
    // contribute to the score. fuzzy gives subsequence + typo tolerance
    let haystacks: Vec<String> = model_completions
        .iter()
        .map(|m| format!("{} {}", m.id, m.name))
        .collect();
    let mut matcher = FuzzyFilter::new();
    let indices = matcher.filter(&haystacks, prefix, String::as_str);
    indices
        .into_iter()
        .map(|i| model_completions[i].clone())
        .collect()
}
