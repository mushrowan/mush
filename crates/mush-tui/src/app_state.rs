use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use mush_ai::types::ToolCallId;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use serde::{Deserialize, Serialize};

use crate::app_event::AppMode;
use crate::login_picker::LoginPickerState;
use crate::message_picker::MessagePickerState;
use crate::model_picker::ModelPickerState;
use crate::session_picker::SessionPickerState;
use crate::slash_menu::{ModelCompletion, SlashCommand, SlashMenuState, TabState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionRecoveryAction {
    Retry,
    Fallback,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct CompactionRecoveryOption {
    pub label: String,
    pub description: String,
    pub action: CompactionRecoveryAction,
}

#[derive(Debug, Clone)]
pub struct CompactionRecoveryState {
    pub reason: String,
    pub selected: usize,
    pub options: Vec<CompactionRecoveryOption>,
    pub failure: mush_session::compact::CompactionFailureReport,
    pub tokens_before: usize,
    pub model_id: String,
}

impl CompactionRecoveryState {
    #[must_use]
    pub fn new(
        failure: mush_session::compact::CompactionFailureReport,
        tokens_before: usize,
        model_id: String,
    ) -> Self {
        let reason = failure.reason().to_string();
        Self {
            reason,
            selected: 0,
            options: vec![
                CompactionRecoveryOption {
                    label: "try again".into(),
                    description: "retry the LLM summary request with the same compacted messages"
                        .into(),
                    action: CompactionRecoveryAction::Retry,
                },
                CompactionRecoveryOption {
                    label: "fallback (structured summary + raw appendix)".into(),
                    description: "use a local structured summary with a raw-message appendix"
                        .into(),
                    action: CompactionRecoveryAction::Fallback,
                },
                CompactionRecoveryOption {
                    label: "cancel compaction".into(),
                    description: "leave the conversation un-compacted".into(),
                    action: CompactionRecoveryAction::Cancel,
                },
            ],
            failure,
            tokens_before,
            model_id,
        }
    }

    pub fn move_down(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.options.len();
    }

    pub fn move_up(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = (self.selected + self.options.len() - 1) % self.options.len();
    }

    #[must_use]
    pub fn selected_action(&self) -> Option<CompactionRecoveryAction> {
        self.options.get(self.selected).map(|option| option.action)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<usize>,
    pub selected: usize,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionState {
    pub slash_menu: Option<SlashMenuState>,
    pub slash_commands: Vec<SlashCommand>,
    pub model_completions: Vec<ModelCompletion>,
    pub completions: Vec<String>,
    pub(crate) tab_state: Option<TabState>,
    pub(crate) history: Option<HistoryCursor>,
    pub favourite_models: Vec<String>,
    pub favourites_locked: bool,
    pub recent_models: Vec<String>,
    pub templates: Vec<mush_ext::PromptTemplate>,
}

#[derive(Debug, Clone)]
pub(crate) struct HistoryCursor {
    pub(crate) index: usize,
    pub(crate) draft: String,
}

#[derive(Debug, Clone)]
pub struct AtPickerState {
    pub matches: Vec<mush_ext::PromptTemplate>,
    pub selected: usize,
    pub trigger_start: usize,
    pub trigger_end: usize,
}

#[derive(Debug, Clone)]
pub struct SlotEditState {
    pub slots: Vec<usize>,
    pub current: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StatusBarConfig {
    pub show_thinking: bool,
    pub show_context: bool,
    pub show_oauth_usage: bool,
    pub show_status_messages: bool,
    pub show_background_alerts: bool,
    pub show_pane_indicator: bool,
    pub show_cwd: bool,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            show_thinking: true,
            show_context: true,
            show_oauth_usage: true,
            show_status_messages: true,
            show_background_alerts: true,
            show_pane_indicator: true,
            show_cwd: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelBusy {
    None,
    Pending,
    Asked,
}

#[derive(Debug)]
pub struct InteractionState {
    pub mode: AppMode,
    pub focus: crate::app_event::Focus,
    pub session_picker: Option<SessionPickerState>,
    pub message_picker: Option<MessagePickerState>,
    pub model_picker: Option<ModelPickerState>,
    pub login_picker: Option<LoginPickerState>,
    pub search: SearchState,
    pub confirm_prompt: Option<String>,
    pub confirm_tool_call_id: Option<ToolCallId>,
    pub branch_summary_ask: Option<mush_session::tree::EntryId>,
    pub kernel_busy: KernelBusy,
    pub feature_hints: bool,
    pub show_cost: bool,
    pub show_usage_lines: bool,
    pub show_token_counters: bool,
    pub status_bar: StatusBarConfig,
    pub at_picker: Option<AtPickerState>,
    pub slot_edit: Option<SlotEditState>,
    pub compaction_recovery: Option<CompactionRecoveryState>,
    pub mouse_scroll_lines: usize,
    pub pending_count: Option<usize>,
}

impl Default for InteractionState {
    fn default() -> Self {
        Self {
            mode: AppMode::Insert,
            focus: crate::app_event::Focus::default(),
            session_picker: None,
            message_picker: None,
            model_picker: None,
            login_picker: None,
            search: SearchState::default(),
            confirm_prompt: None,
            confirm_tool_call_id: None,
            branch_summary_ask: None,
            kernel_busy: KernelBusy::None,
            feature_hints: true,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar: StatusBarConfig::default(),
            mouse_scroll_lines: 1,
            pending_count: None,
            at_picker: None,
            slot_edit: None,
            compaction_recovery: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub text: String,
}

impl<T: Into<String>> From<T> for Status {
    fn from(text: T) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NavigationState {
    pub has_unread: bool,
}

pub type CachedToolBoxLines = (u64, u16, Vec<Line<'static>>);

#[derive(Debug, Clone)]
pub struct CachedStatusBar {
    pub width: u16,
    pub spans: Vec<ratatui::text::Span<'static>>,
    pub right_text: String,
    pub cache_timer_text: Option<String>,
    pub height: u16,
}

#[derive(Debug)]
pub struct RenderState {
    pub message_area: Cell<Rect>,
    pub markdown_cache: RefCell<HashMap<String, Text<'static>>>,
    pub stream_markdown_cache: RefCell<Option<(usize, String, Text<'static>)>>,
    pub tool_box_cache: RefCell<Vec<Option<CachedToolBoxLines>>>,
    pub status_bar_cache: RefCell<Option<CachedStatusBar>>,
}

impl RenderState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            message_area: Cell::new(Rect::default()),
            markdown_cache: RefCell::new(HashMap::new()),
            stream_markdown_cache: RefCell::new(None),
            tool_box_cache: RefCell::new(Vec::new()),
            status_bar_cache: RefCell::new(None),
        }
    }
}

impl Default for RenderState {
    fn default() -> Self {
        Self::new()
    }
}
