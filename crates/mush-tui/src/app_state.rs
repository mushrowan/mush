//! grouped substates for the tui app

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use mush_ai::types::ToolCallId;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use serde::{Deserialize, Serialize};

use crate::app_event::AppMode;
use crate::login_picker::LoginPickerState;
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

/// state for the conversation search popup
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// current search query
    pub query: String,
    /// indices of matching messages
    pub matches: Vec<usize>,
    /// which match is currently selected
    pub selected: usize,
}

/// slash completion and tab cycling state
#[derive(Debug, Clone, Default)]
pub struct CompletionState {
    /// slash command menu state
    pub slash_menu: Option<SlashMenuState>,
    /// registered slash commands with descriptions
    pub slash_commands: Vec<SlashCommand>,
    /// available models for /model menu
    pub model_completions: Vec<ModelCompletion>,
    /// available completions (slash commands, model ids, etc)
    pub completions: Vec<String>,
    /// current tab-completion state
    pub(crate) tab_state: Option<TabState>,
    /// effective favourite model ids (from config + disk, resolved at startup)
    pub favourite_models: Vec<String>,
    /// whether favourites are pinned by config.toml
    pub favourites_locked: bool,
    /// model ids in most-recently-used order (most recent first). a
    /// model counts as used when a message is actually sent to it, so
    /// the favourites tab can sort by recency without alt+m churn
    pub recent_models: Vec<String>,
    /// loaded prompt templates, used for `@name<tab>` expansion.
    /// populated at runtime from user + project prompts directories
    /// (`~/.config/mush/prompts/`, `.mush/prompts/`)
    pub templates: Vec<mush_ext::PromptTemplate>,
}

/// state for the `@`-template picker popup. opens when the user presses
/// tab on an `@<word>` that doesn't match any template exactly but does
/// have one or more prefix matches. tab/shift+tab cycle, enter inserts,
/// esc closes without touching the input
#[derive(Debug, Clone)]
pub struct AtPickerState {
    /// templates that prefix-match the trigger word, in source order
    pub matches: Vec<mush_ext::PromptTemplate>,
    /// which match is selected
    pub selected: usize,
    /// byte offset of the `@` sign in the input. used to know where the
    /// trigger word starts so insertion can replace `@<word>` with the
    /// template content
    pub trigger_start: usize,
    /// byte offset of the cursor at trigger time, i.e. the end of the
    /// `@<word>` token. captured up-front so the replace range stays
    /// stable even if the user types into the input while the picker is
    /// open (which closes it without inserting)
    pub trigger_end: usize,
}

/// state for the interactive slot editor that fills in `$1`/`$2`/`$@`
/// placeholders after a template expands. tab cycles between slot byte
/// offsets in `input.text`; typing/backspace at the current slot shifts
/// the later slot offsets to keep them aligned with the live text
#[derive(Debug, Clone)]
pub struct SlotEditState {
    /// byte offsets in `input.text` where each slot lives. updated as
    /// the user types so later slots stay aligned with their actual
    /// position in the input
    pub slots: Vec<usize>,
    /// index of the slot the cursor is currently in
    pub current: usize,
}

/// toggles for individual status bar segments
///
/// everything defaults on so existing setups are unaffected. per-field
/// `show_cost` and `show_token_counters` live on `InteractionState`
/// (separately toggleable at runtime via `/cost`), not here
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StatusBarConfig {
    /// `thinking: <level>` segment
    pub show_thinking: bool,
    /// `<used>/<window>` context + cache warmth segment
    pub show_context: bool,
    /// oauth 5h / 7d usage bars (claude code plan)
    pub show_oauth_usage: bool,
    /// ephemeral status messages from slash commands, tools, etc
    pub show_status_messages: bool,
    /// background pane notifications
    pub show_background_alerts: bool,
    /// `[i/n]` pane indicator when multi-pane
    pub show_pane_indicator: bool,
    /// cwd in the right column
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

/// modal and prompt-related ui state
#[derive(Debug)]
pub struct InteractionState {
    /// which UI mode we're in
    pub mode: AppMode,
    /// session picker state (when mode == SessionPicker)
    pub session_picker: Option<SessionPickerState>,
    /// model picker state (when mode == ModelPicker)
    pub model_picker: Option<ModelPickerState>,
    /// login picker state (when mode == LoginPicker)
    pub login_picker: Option<LoginPickerState>,
    /// search state
    pub search: SearchState,
    /// tool confirmation prompt (shown when mode == ToolConfirm)
    pub confirm_prompt: Option<String>,
    /// tool call being confirmed
    pub confirm_tool_call_id: Option<ToolCallId>,
    /// whether to show dollar cost in status bar
    pub show_cost: bool,
    /// whether to render per-message usage lines (off by default, same info is in status bar)
    pub show_usage_lines: bool,
    /// whether to show the ↑/↓/R/W token counters in the status bar (off by default)
    pub show_token_counters: bool,
    /// per-segment visibility toggles for the status bar
    pub status_bar: StatusBarConfig,
    /// `@`-template picker state, populated when the user pressed tab
    /// on a partial-match trigger. presence implies `mode == AtPicker`
    pub at_picker: Option<AtPickerState>,
    /// slot editor state, populated when an expanded template contains
    /// `$1`/`$2`/`$@` placeholders. presence implies `mode == SlotEdit`
    pub slot_edit: Option<SlotEditState>,
    /// recovery prompt shown when LLM compaction fails before falling
    /// back. presence implies `mode == CompactionRecovery`
    pub compaction_recovery: Option<CompactionRecoveryState>,
}

impl Default for InteractionState {
    fn default() -> Self {
        Self {
            mode: AppMode::Normal,
            session_picker: None,
            model_picker: None,
            login_picker: None,
            search: SearchState::default(),
            confirm_prompt: None,
            confirm_tool_call_id: None,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar: StatusBarConfig::default(),
            at_picker: None,
            slot_edit: None,
            compaction_recovery: None,
        }
    }
}

/// selection and scroll navigation state
#[derive(Debug, Clone, Default)]
pub struct NavigationState {
    /// new messages arrived while the terminal was unfocused
    pub has_unread: bool,
}

/// cached rendered lines for completed tool boxes.
/// keyed by (content_hash, width) so entries invalidate on tool content
/// changes or resize.
pub type CachedToolBoxLines = (u64, u16, Vec<Line<'static>>);

/// cached per-frame status bar data so `left_spans` (which issues
/// ~10 format! calls) and `status_bar_height` + `StatusBar::render`
/// only pay the cost once per frame instead of 3-4 times
#[derive(Debug, Clone)]
pub struct CachedStatusBar {
    pub width: u16,
    pub spans: Vec<ratatui::text::Span<'static>>,
    pub right_text: String,
    pub cache_timer_text: Option<String>,
    pub confirm: Option<String>,
    pub height: u16,
}

/// render caches and geometry from the last frame
#[derive(Debug)]
pub struct RenderState {
    /// message area rect from last render
    pub message_area: Cell<Rect>,
    /// cached markdown rendering for stable message content
    pub markdown_cache: RefCell<HashMap<String, Text<'static>>>,
    /// cached markdown rendering for the current visible streaming text.
    /// keyed by `(message_width, full_stream_text)` because tables wrap
    /// to the message width and must reflow after resizes
    pub stream_markdown_cache: RefCell<Option<(usize, String, Text<'static>)>>,
    /// per-message rendered completed-tool box cache.
    /// avoids re-running syntect over stable tool summaries and previews
    /// every frame.
    pub tool_box_cache: RefCell<Vec<Option<CachedToolBoxLines>>>,
    /// per-frame status bar cache. cleared at the start of each frame so
    /// stale stats never leak. within a frame all `status_bar_height`,
    /// `StatusBar::render` and `Ui::cursor_position` callers share the
    /// same built spans instead of rebuilding with ~10 format! calls
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
