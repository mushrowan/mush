//! opening and closing the modal overlays
//!
//! each picker keeps its state in [`crate::app_state::InteractionState`]
//! alongside the mode tag, so open and close have to move both together.

use super::*;

impl App {
    /// open the slash command completion menu, filtering by current input
    pub fn open_slash_menu(&mut self) {
        let prefix = self.input.text.as_str();
        let matches = filter_command_matches(&self.completion.slash_commands, prefix);
        if matches.is_empty() {
            return;
        }

        self.completion.slash_menu = Some(SlashMenuState::for_commands(matches));
        self.interaction.mode = AppMode::SlashComplete;
    }

    /// update the slash menu filter based on current input
    pub fn update_slash_menu(&mut self) {
        if let Some(ref mut menu) = self.completion.slash_menu {
            let prefix = self.input.text.as_str();
            menu.matches = filter_command_matches(&self.completion.slash_commands, prefix);
            menu.selected = 0;

            if menu.matches.is_empty() {
                self.close_slash_menu();
            }
        }
    }

    /// close the slash menu and return to normal mode
    pub fn close_slash_menu(&mut self) {
        self.completion.slash_menu = None;
        self.interaction.mode = AppMode::Normal;
    }

    /// open the session picker with the given sessions
    pub fn open_session_picker(&mut self, sessions: Vec<SessionMeta>, cwd: String) {
        self.interaction.session_picker = Some(SessionPickerState::new(sessions, cwd));
        self.interaction.mode = AppMode::SessionPicker;
    }

    /// open the session picker backed by a streaming loader. sessions
    /// arrive via the receiver and are drained into the picker by the
    /// main event loop tick, so the overlay is visible immediately and
    /// fills in as metadata parses
    pub fn open_session_picker_streaming(
        &mut self,
        incoming: std::sync::mpsc::Receiver<SessionMeta>,
        cwd: String,
    ) {
        self.interaction.session_picker = Some(SessionPickerState::new_streaming(incoming, cwd));
        self.interaction.mode = AppMode::SessionPicker;
    }

    /// close the session picker
    pub fn close_session_picker(&mut self) {
        self.interaction.session_picker = None;
        self.interaction.mode = AppMode::Normal;
    }

    /// get the currently selected session id (if picker is open)
    pub fn selected_session(&self) -> Option<&SessionMeta> {
        let picker = self.interaction.session_picker.as_ref()?;
        let filtered = filtered_sessions(picker);
        filtered.get(picker.selected).copied()
    }

    /// open the centred model picker overlay. mirrors
    /// [`Self::open_session_picker`] but for models, replacing the
    /// old slash-menu-with-`model_mode` flow. seeds favourites and
    /// the lock state from the user's current config so ★ markers
    /// and ctrl+f behaviour reflect their settings.
    pub fn open_model_picker_overlay(&mut self) {
        self.open_model_picker_overlay_inner(false);
    }

    /// open the model picker with codex's hidden entries (`internal`,
    /// `experimental`) included. routed from `/model --all`.
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

    /// close the model picker overlay
    pub fn close_model_picker(&mut self) {
        self.interaction.model_picker = None;
        self.interaction.mode = AppMode::Normal;
    }

    /// id of the highlighted model in the open picker, if any
    pub fn selected_model_id(&self) -> Option<String> {
        let picker = self.interaction.model_picker.as_ref()?;
        let filtered = crate::model_picker::filtered_models(picker);
        Some(filtered.get(picker.selected)?.id.clone())
    }

    /// open the centred login picker overlay. mirrors
    /// [`Self::open_session_picker`] / [`Self::open_model_picker_overlay`]
    /// but for login providers. `config_keys` is snapshotted onto the
    /// picker so source detection still works after a login/logout
    /// without re-plumbing the runner
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

    /// close the login picker overlay
    pub fn close_login_picker(&mut self) {
        self.interaction.login_picker = None;
        self.interaction.mode = AppMode::Normal;
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
        self.interaction.mode = AppMode::Normal;
    }

    /// provider id of the highlighted row in the open login picker, if any
    pub fn selected_login_provider(&self) -> Option<String> {
        let picker = self.interaction.login_picker.as_ref()?;
        let filtered = crate::login_picker::filtered_entries(picker);
        Some(filtered.get(picker.selected)?.id.clone())
    }
}
