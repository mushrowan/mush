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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caret {
    ComposerBar,
    ComposerBlock,
    Log,
}

impl Caret {
    #[must_use]
    pub fn is_block(self) -> bool {
        matches!(self, Self::ComposerBlock | Self::Log)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    ComposerNormal,
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

impl AppMode {
    #[must_use]
    pub fn scrolls_the_log(&self) -> bool {
        matches!(self, Self::Normal | Self::ComposerNormal | Self::Navigate)
    }

    #[must_use]
    pub fn caret(&self) -> Caret {
        match self {
            Self::Navigate => Caret::Log,
            Self::ComposerNormal => Caret::ComposerBlock,
            _ => Caret::ComposerBar,
        }
    }
}
