#[derive(Debug, Clone)]
pub enum AppEvent {
    UserSubmit {
        text: String,
        images: Vec<crate::input_buffer::PendingImage>,
    },
    SlashCommand {
        action: crate::slash::SlashAction,
    },
    SettingsToggleSelected,
    ModelSelected {
        model_id: String,
    },
    PersistFavourites,
    Quit,
    Abort,
    ScrollUp(u16),
    ScrollDown(u16),
    Resize(u16, u16),
    CycleThinkingLevel,
    PasteImage,
    EditSteering,
    ValidateAndSaveApiKey {
        entry_id: String,
        provider_name: String,
        key: String,
    },
    CompactionRecoverySelected {
        action: crate::app_state::CompactionRecoveryAction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    SessionPicker,
    ModelPicker,
    LoginPicker,
    SlashComplete,
    AtPicker,
    SlotEdit,
    ToolConfirm,
    Navigate,
    Search,
    Settings,
    CompactionRecovery,
}
