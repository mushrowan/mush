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
    ExternalEditor,
    ValidateAndSaveApiKey {
        entry_id: String,
        provider_name: String,
        key: String,
    },
    Copy {
        text: String,
    },
    PutClipboard {
        before: bool,
        count: Option<usize>,
    },
    CompactionRecoverySelected {
        action: crate::app_state::CompactionRecoveryAction,
    },
    DecisionDrafts(Vec<(mush_runtime::DecisionKey, Option<mush_runtime::Answer>)>),
    DecisionsSend {
        keys: Vec<mush_runtime::DecisionKey>,
        confirms: Vec<(String, bool)>,
    },
    DecisionsRefresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Composer,
    Log,
}

impl Focus {
    #[must_use]
    pub fn is_log(self) -> bool {
        matches!(self, Self::Log)
    }
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
    Insert,
    Normal,
    SessionPicker,
    MessagePicker,
    ModelPicker,
    LoginPicker,
    SlashComplete,
    AtPicker,
    SlotEdit,
    ToolConfirm,
    Visual,
    Search,
    Settings,
    CompactionRecovery,
}

impl AppMode {
    #[must_use]
    pub fn scrolls_the_log(&self) -> bool {
        matches!(self, Self::Insert | Self::Normal | Self::Visual)
    }

    #[must_use]
    pub fn caret(&self, focus: Focus) -> Caret {
        match (self, focus) {
            (Self::Normal | Self::Visual, Focus::Log) => Caret::Log,
            (Self::Normal | Self::Visual, Focus::Composer) => Caret::ComposerBlock,
            _ => Caret::ComposerBar,
        }
    }
}
