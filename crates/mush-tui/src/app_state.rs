//! grouped substates for the tui app

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use mush_ai::types::ToolCallId;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use serde::{Deserialize, Serialize};

use crate::app::ScrollUnit;
use crate::app_event::AppMode;
use crate::display_types::{ImageRenderArea, MessageRowRange};
use crate::session_picker::SessionPickerState;
use crate::slash_menu::{ModelCompletion, SlashCommand, SlashMenuState, TabState};

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
    /// `N%` percent-from-top when scrolled up
    pub show_scroll_position: bool,
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
            show_scroll_position: true,
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
    /// search state
    pub search: SearchState,
    /// tool confirmation prompt (shown when mode == ToolConfirm)
    pub confirm_prompt: Option<String>,
    /// tool call being confirmed
    pub confirm_tool_call_id: Option<ToolCallId>,
    /// whether to show prompt injection previews in the chat
    pub show_prompt_injection: bool,
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
}

impl Default for InteractionState {
    fn default() -> Self {
        Self {
            mode: AppMode::Normal,
            session_picker: None,
            search: SearchState::default(),
            confirm_prompt: None,
            confirm_tool_call_id: None,
            show_prompt_injection: false,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar: StatusBarConfig::default(),
            at_picker: None,
        }
    }
}

/// selection and scroll navigation state
#[derive(Debug, Clone, Default)]
pub struct NavigationState {
    /// new messages arrived while scrolled up
    pub has_unread: bool,
    /// selected message index in scroll mode (for copy)
    pub selected_message: Option<usize>,
    /// anchor for visual selection range (v in scroll mode)
    pub selection_anchor: Option<usize>,
    /// what j/k navigates in scroll mode
    pub scroll_unit: ScrollUnit,
    /// selected code block index in block scroll mode
    pub selected_block: Option<usize>,
}

/// cached indented lines for a single message.
/// keyed by (content_hash, width) so entries invalidate on content or resize.
type CachedIndentedLines = (u64, u16, Vec<Line<'static>>);

/// cached estimated height for a single message.
/// keyed by cheap O(1) byte-length fingerprint so we avoid
/// re-scanning content with chars().count() every frame
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedHeight {
    pub content_len: usize,
    pub thinking_len: usize,
    pub tool_output_len: usize,
    pub completed_tool_count: usize,
    pub thinking_expanded: bool,
    pub has_usage: bool,
    pub width: u16,
    pub height: usize,
}

/// cached per-frame status bar data so `left_spans` (which issues
/// ~10 format! calls) and `status_bar_height` + `StatusBar::render`
/// only pay the cost once per frame instead of 3-4 times
#[derive(Debug, Clone)]
pub struct CachedStatusBar {
    pub width: u16,
    pub spans: Vec<ratatui::text::Span<'static>>,
    pub right_text: String,
    pub confirm: Option<String>,
    pub height: u16,
}

/// render caches and geometry from the last frame
#[derive(Debug)]
pub struct RenderState {
    /// image render positions (populated by MessageList during render)
    pub image_render_areas: RefCell<Vec<ImageRenderArea>>,
    /// per-message wrapped-line ranges (populated by MessageList during render)
    pub message_row_ranges: RefCell<Vec<MessageRowRange>>,
    /// message area rect from last render
    pub message_area: Cell<Rect>,
    /// scroll value from last render (wrapped lines scrolled past)
    pub render_scroll: Cell<u16>,
    /// cached markdown rendering for stable message content
    pub markdown_cache: RefCell<HashMap<String, Text<'static>>>,
    /// cached markdown rendering for the current visible streaming text
    pub stream_markdown_cache: RefCell<Option<(String, Text<'static>)>>,
    /// total content lines (set during render by MessageList)
    pub total_content_lines: Cell<u16>,
    /// visible area height (set during render by MessageList)
    pub visible_area_height: Cell<u16>,
    /// per-message indented lines cache.
    /// stores (content_hash, width, pre-indented lines) so stable
    /// messages skip both the markdown clone and indent_line computation
    pub indented_cache: RefCell<Vec<Option<CachedIndentedLines>>>,
    /// per-message estimated height cache.
    /// avoids re-running count_estimated_lines (which does chars().count()
    /// per line) on every frame for stable messages
    pub height_cache: RefCell<Vec<Option<CachedHeight>>>,
    /// per-message content hash cache.
    /// avoids rehashing full content string every frame when checking
    /// indented cache validity. keyed by byte length (O(1) check)
    pub content_hash_cache: RefCell<Vec<Option<(usize, u64)>>>,
    /// total content lines (actual) from the previous render frame.
    /// used to anchor scroll compensation during content fluctuation
    pub prev_content_lines: Cell<usize>,
    /// actual total content lines at the moment the user scrolled up
    /// from the bottom. while `scroll_offset > 0`, compensation keeps
    /// the viewport pinned to this baseline by offsetting scroll by
    /// `current_actual_total - baseline`. reset to 0 when scroll_offset
    /// returns to 0 so the next scroll-up re-captures the baseline.
    /// this makes the viewport stable against estimate/actual drift
    /// during streaming (the previous estimate-based delta approach
    /// accumulated errors whenever count_estimated_lines disagreed
    /// with markdown-rendered line count)
    pub scroll_baseline: Cell<usize>,
    /// scroll compensation (in lines) exposed for the status bar
    /// and tests. derived post-render as `total_content_lines -
    /// scroll_baseline` while scrolled up, otherwise 0. signed so
    /// shrinkage past the baseline is handled correctly
    pub scroll_compensation: Cell<isize>,
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
            image_render_areas: RefCell::new(Vec::new()),
            message_row_ranges: RefCell::new(Vec::new()),
            message_area: Cell::new(Rect::default()),
            render_scroll: Cell::new(0),
            markdown_cache: RefCell::new(HashMap::new()),
            stream_markdown_cache: RefCell::new(None),
            total_content_lines: Cell::new(0),
            visible_area_height: Cell::new(0),
            indented_cache: RefCell::new(Vec::new()),
            height_cache: RefCell::new(Vec::new()),
            content_hash_cache: RefCell::new(Vec::new()),
            prev_content_lines: Cell::new(0),
            scroll_baseline: Cell::new(0),
            scroll_compensation: Cell::new(0),
            status_bar_cache: RefCell::new(None),
        }
    }
}

impl Default for RenderState {
    fn default() -> Self {
        Self::new()
    }
}
