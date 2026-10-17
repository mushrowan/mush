use super::*;

impl App {
    pub fn open_slash_menu(&mut self) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }

        self.completion.slash_menu = Some(SlashMenuState::for_commands(matches));
        self.interaction.mode = AppMode::SlashComplete;
    }

    pub fn update_slash_menu(&mut self) {
        let matches = self.slash_matches();
        if let Some(ref mut menu) = self.completion.slash_menu {
            menu.matches = matches;
            menu.selected = 0;

            if menu.matches.is_empty() {
                self.close_slash_menu();
            }
        }
    }

    fn slash_matches(&self) -> Vec<crate::slash_menu::SlashCommand> {
        let text = self.input.text.as_str();
        match crate::slash_menu::argument_prefix(text) {
            Some((command, prefix)) => self.argument_matches(command, prefix),
            None => filter_command_matches(&self.completion.slash_commands, text),
        }
    }

    fn argument_matches(
        &self,
        command: &str,
        prefix: &str,
    ) -> Vec<crate::slash_menu::SlashCommand> {
        match command {
            "effort" | "thinking" => {
                let current = self.thinking_level.normalize_visible();
                self.available_thinking_levels()
                    .into_iter()
                    .filter_map(|level| {
                        let name = level.effort_name().unwrap_or("off").to_string();
                        name.starts_with(prefix).then(|| {
                            let description = crate::slash_menu::thinking_level_description(&name);
                            crate::slash_menu::SlashCommand {
                                name: format!("{command} {name}"),
                                description: if level == current {
                                    format!("{description} (current)")
                                } else {
                                    description.to_string()
                                },
                            }
                        })
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    pub fn close_slash_menu(&mut self) {
        self.completion.slash_menu = None;
        self.interaction.mode = AppMode::Insert;
    }

    pub fn open_session_picker(&mut self, sessions: Vec<SessionMeta>, cwd: String) {
        self.interaction.session_picker = Some(SessionPickerState::new(sessions, cwd));
        self.interaction.mode = AppMode::SessionPicker;
    }

    pub fn open_session_picker_streaming(
        &mut self,
        incoming: std::sync::mpsc::Receiver<SessionMeta>,
        cwd: String,
    ) {
        self.interaction.session_picker = Some(SessionPickerState::new_streaming(incoming, cwd));
        self.interaction.mode = AppMode::SessionPicker;
    }

    pub fn close_session_picker(&mut self) {
        self.interaction.session_picker = None;
        self.interaction.mode = AppMode::Insert;
    }

    pub fn open_message_picker(
        &mut self,
        purpose: crate::message_picker::MessagePickerPurpose,
        items: Vec<crate::message_picker::MessagePickerItem>,
    ) {
        self.interaction.message_picker = Some(crate::message_picker::MessagePickerState::new(
            purpose, items,
        ));
        self.interaction.mode = AppMode::MessagePicker;
    }

    pub fn close_message_picker(&mut self) {
        self.interaction.message_picker = None;
        self.interaction.mode = AppMode::Insert;
    }

    pub fn selected_session(&self) -> Option<&SessionMeta> {
        let picker = self.interaction.session_picker.as_ref()?;
        let filtered = filtered_sessions(picker);
        filtered.get(picker.selected).copied()
    }

    pub fn open_model_picker_overlay(&mut self) {
        self.open_model_picker_overlay_inner(false);
    }

    pub fn open_subagent_model_picker(&mut self) {
        self.open_subagent_picker_for(crate::model_picker::ModelPickerPurpose::Subagent);
    }

    pub fn open_subagent_default_picker(&mut self) {
        self.open_subagent_picker_for(crate::model_picker::ModelPickerPurpose::SubagentDefault);
    }

    fn open_subagent_picker_for(&mut self, purpose: crate::model_picker::ModelPickerPurpose) {
        self.open_model_picker_overlay_inner(false);
        if let Some(picker) = self.interaction.model_picker.as_mut() {
            picker.purpose = purpose;
            picker.tab = crate::model_picker::ModelPickerTab::All;
            picker.models.insert(0, crate::model_picker::inherit_row());
            picker.selected = 0;
        }
    }

    pub fn open_model_picker_with_search(&mut self, search: &str) -> bool {
        self.open_model_picker_overlay_inner(false);
        let Some(picker) = self.interaction.model_picker.as_mut() else {
            return false;
        };
        picker.tab = crate::model_picker::ModelPickerTab::All;
        picker.filter = search.trim().to_string();
        picker.selected = 0;
        true
    }

    pub fn open_model_picker_overlay_all(&mut self) {
        self.open_model_picker_overlay_inner(true);
    }

    fn open_model_picker_overlay_inner(&mut self, show_all: bool) {
        let models =
            crate::slash_menu::prepare_picker_models(&self.completion.model_completions, show_all);
        if models.is_empty() {
            return;
        }
        let favourites = crate::model_picker::favourites_by_recency(
            &self.completion.favourite_models,
            &self.completion.recent_models,
        );
        let state = crate::model_picker::ModelPickerState::new(
            models,
            favourites,
            self.completion.favourites_locked,
            show_all,
        );
        self.interaction.model_picker = Some(state);
        self.interaction.mode = AppMode::ModelPicker;
    }

    pub fn close_model_picker(&mut self) {
        self.interaction.model_picker = None;
        self.interaction.mode = AppMode::Insert;
    }

    pub fn selected_model_id(&self) -> Option<String> {
        let picker = self.interaction.model_picker.as_ref()?;
        let filtered = crate::model_picker::filtered_models(picker);
        Some(filtered.get(picker.selected)?.id.clone())
    }

    pub fn open_login_picker(
        &mut self,
        entries: Vec<crate::login_picker::LoginEntry>,
        config_keys: std::collections::HashMap<String, mush_ai::types::ApiKey>,
    ) {
        self.interaction.login_picker = Some(
            crate::login_picker::LoginPickerState::with_config_keys(entries, config_keys),
        );
        self.interaction.mode = AppMode::LoginPicker;
    }

    pub fn close_login_picker(&mut self) {
        self.interaction.login_picker = None;
        self.interaction.mode = AppMode::Insert;
    }

    pub fn open_compaction_recovery(
        &mut self,
        failure: mush_session::compact::CompactionFailureReport,
        tokens_before: usize,
        model_id: String,
    ) {
        self.compacting = false;
        self.status = Some("compaction needs input".into());
        self.interaction.compaction_recovery = Some(CompactionRecoveryState::new(
            failure,
            tokens_before,
            model_id,
        ));
        self.interaction.mode = AppMode::CompactionRecovery;
    }

    pub fn close_compaction_recovery(&mut self) {
        self.interaction.compaction_recovery = None;
        self.interaction.mode = AppMode::Insert;
    }

    pub fn selected_login_provider(&self) -> Option<String> {
        let picker = self.interaction.login_picker.as_ref()?;
        let filtered = crate::login_picker::filtered_entries(picker);
        Some(filtered.get(picker.selected)?.id.clone())
    }
}
