//! grouped substates for the tui app

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use mush_ai::types::ToolCallId;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};

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
}

/// modal and prompt-related ui state
#[derive(Debug, Clone)]
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
        }
    }
}

impl Default for RenderState {
    fn default() -> Self {
        Self::new()
    }
}
