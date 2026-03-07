//! app state and event loop
//!
//! the app holds all TUI state: messages being displayed, current input,
//! streaming status, and scroll position.

use std::cell::{Cell, RefCell};

use mush_ai::types::*;
use mush_session::SessionMeta;
use ratatui::layout::Rect;
use throbber_widgets_tui::ThrobberState;

use crate::clipboard::ClipboardImage;

/// an image attached to the next user message (not yet sent)
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub data: Vec<u8>,
    pub mime_type: ImageMimeType,
    /// image dimensions (width, height) if decoded
    pub dimensions: Option<(u32, u32)>,
}

/// object replacement character, marks image positions in input text.
/// each occurrence maps to the Nth entry in pending_images (by order)
pub const IMAGE_PLACEHOLDER: char = '\u{FFFC}';

/// events that flow between the TUI and the agent
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// user submitted a prompt
    UserSubmit {
        text: String,
    },
    /// user executed a slash command
    SlashCommand {
        name: String,
        args: String,
    },
    /// user requested quit
    Quit,
    /// user requested abort of current operation
    Abort,
    /// user scrolled up/down
    ScrollUp(u16),
    ScrollDown(u16),
    /// resize
    Resize(u16, u16),
    /// user cycled thinking level
    CycleThinkingLevel,
    /// user triggered clipboard image paste
    PasteImage,
    /// split current pane (fork conversation into new agent)
    SplitPane,
    /// close the focused pane
    ClosePane,
    /// focus the next pane
    FocusNextPane,
    /// focus the previous pane
    FocusPrevPane,
    /// focus pane by index (0-based)
    FocusPaneByIndex(usize),
    /// resize focused pane (positive = grow, negative = shrink)
    ResizePane(i16),
}

/// controls how thinking text is displayed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    /// never show thinking text
    Hidden,
    /// show while streaming, collapse to one-line preview when done
    Collapse,
    /// always show thinking text expanded
    #[default]
    Expanded,
}

/// which UI mode the app is in
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    SessionPicker,
    /// slash command completion menu visible above input
    SlashComplete,
    /// waiting for user to confirm a tool call (y/n)
    ToolConfirm,
    /// scroll mode: j/k scroll, y copies message, esc exits
    Scroll,
    /// search mode: type to filter messages, enter to jump
    Search,
}

/// session picker scope
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    /// sessions from current working directory only
    ThisDir,
    /// all sessions across all directories
    AllDirs,
}

/// state for the session picker overlay
#[derive(Debug, Clone)]
pub struct SessionPickerState {
    pub sessions: Vec<SessionMeta>,
    pub selected: usize,
    pub filter: String,
    pub scope: SessionScope,
    /// current working directory for scope filtering
    pub cwd: String,
}

/// a displayable message block in the conversation
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: MessageRole,
    pub content: String,
    pub tool_calls: Vec<DisplayToolCall>,
    pub thinking: Option<String>,
    /// whether thinking is expanded (visible)
    pub thinking_expanded: bool,
    pub usage: Option<Usage>,
    pub cost: Option<f64>,
    /// model id for assistant messages
    pub model_id: Option<ModelId>,
    /// whether this message is queued (steering) and hasn't been processed yet
    pub queued: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
pub struct DisplayToolCall {
    pub name: String,
    pub summary: String,
    pub status: ToolCallStatus,
    /// truncated preview of tool output
    pub output_preview: Option<String>,
    /// raw image bytes from tool result (for inline rendering)
    pub image_data: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Done,
    Error,
}

/// state for a currently executing tool (shown in the tool panels)
#[derive(Debug, Clone)]
pub struct ActiveToolState {
    pub tool_call_id: String,
    pub name: String,
    pub summary: String,
    pub live_output: Option<String>,
    pub status: ToolCallStatus,
    /// output text (set when tool completes)
    pub output: Option<String>,
}

/// cumulative token and cost tracking for the session
#[derive(Debug, Clone, Default)]
pub struct TokenStats {
    /// total cost so far
    pub total_cost: f64,
    /// total tokens used (cumulative across all API calls)
    pub total_tokens: u64,
    /// cumulative uncached input tokens
    pub input_tokens: u64,
    /// cumulative output tokens
    pub output_tokens: u64,
    /// cumulative cache-read tokens
    pub cache_read_tokens: u64,
    /// cumulative cache-write tokens
    pub cache_write_tokens: u64,
    /// last call's input tokens (actual context size)
    pub context_tokens: u64,
    /// model's context window size
    pub context_window: u64,
}

impl TokenStats {
    /// create with a given context window
    #[must_use]
    pub fn new(context_window: u64) -> Self {
        Self {
            context_window,
            ..Default::default()
        }
    }

    /// accumulate usage from an API call
    pub fn update(&mut self, usage: &Usage, cost: Option<f64>) {
        if let Some(c) = cost {
            self.total_cost += c;
        }
        self.total_tokens += usage.total_tokens();
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens;
        self.cache_write_tokens += usage.cache_write_tokens;
        self.context_tokens = usage.total_input_tokens();
    }

    /// reset all counters (keeps context_window)
    pub fn reset(&mut self) {
        let window = self.context_window;
        *self = Self::new(window);
    }
}

/// the main app state
pub struct App {
    /// conversation messages for display
    pub messages: Vec<DisplayMessage>,
    /// current text being streamed in
    pub streaming_text: String,
    /// current thinking being streamed in
    pub streaming_thinking: String,
    /// whether we're currently streaming a response
    pub is_streaming: bool,
    /// user input buffer
    pub input: String,
    /// cursor position in input
    pub cursor: usize,
    /// vertical scroll offset (lines from bottom)
    pub scroll_offset: u16,
    /// model id being used
    pub model_id: ModelId,
    /// token and cost tracking
    pub stats: TokenStats,
    /// whether we should quit
    pub should_quit: bool,
    /// status message (bottom bar)
    pub status: Option<String>,
    /// currently executing tools (for side-by-side panel display)
    pub active_tools: Vec<ActiveToolState>,
    /// tool args streaming in (partial JSON from ToolCallDelta)
    pub streaming_tool_args: String,
    /// chars of streaming_text currently visible (typewriter effect)
    visible_text_chars: usize,
    /// chars of streaming_thinking currently visible (typewriter effect)
    visible_thinking_chars: usize,
    /// spinner state for animations
    pub throbber_state: ThrobberState,
    /// frame counter for throttling spinner speed
    tick_count: u8,
    /// current thinking level
    pub thinking_level: ThinkingLevel,
    /// how to display thinking text
    pub thinking_display: ThinkingDisplay,
    /// which UI mode we're in
    pub mode: AppMode,
    /// session picker state (when mode == SessionPicker)
    pub session_picker: Option<SessionPickerState>,
    /// slash command menu state (when mode == SlashComplete)
    pub slash_menu: Option<SlashMenuState>,
    /// registered slash commands with descriptions
    pub slash_commands: Vec<SlashCommand>,
    /// available models for /model menu
    pub model_completions: Vec<ModelCompletion>,
    /// available completions (slash commands, model ids, etc)
    pub completions: Vec<String>,
    /// current tab-completion state
    tab_state: Option<TabState>,
    /// new messages arrived while scrolled up
    pub has_unread: bool,
    /// tool confirmation prompt (shown when mode == ToolConfirm)
    pub confirm_prompt: Option<String>,
    /// whether to show prompt injection previews in the chat
    pub show_prompt_injection: bool,
    /// whether to show dollar cost in status bar
    pub show_cost: bool,
    /// selected message index in scroll mode (for copy)
    pub selected_message: Option<usize>,
    /// search state
    pub search: SearchState,
    /// image render positions (populated by MessageList during render)
    pub image_render_areas: RefCell<Vec<ImageRenderArea>>,
    /// images attached to the next user message (not yet sent)
    pub pending_images: Vec<PendingImage>,
    /// working directory (with ~ for home)
    pub cwd: String,
    /// total content lines (set during render by MessageList)
    pub total_content_lines: Cell<u16>,
    /// visible area height (set during render by MessageList)
    pub visible_area_height: Cell<u16>,
    /// current input scroll offset (lines from top)
    pub input_scroll: Cell<u16>,
    /// total wrapped input lines (set during render by InputBox)
    pub input_total_lines: Cell<u16>,
    /// visible input lines excluding borders (set during render by InputBox)
    pub input_visible_lines: Cell<u16>,
    /// latest input area rect (set during render by Ui)
    pub input_area: Cell<Rect>,
    /// pane info: (this pane index 1-based, total panes), None when single pane
    pub pane_info: Option<(u16, u16)>,
    /// background pane alert text (e.g. "pane 2: busy")
    pub background_alert: Option<String>,
}

/// position computed during render for inline image overlay
#[derive(Debug, Clone)]
pub struct ImageRenderArea {
    pub msg_idx: usize,
    pub tc_idx: usize,
    pub area: Rect,
}

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

/// tracks an in-progress tab completion cycle
#[derive(Debug, Clone)]
struct TabState {
    /// matching candidates
    matches: Vec<String>,
    /// which match we're showing (cycles on repeated tab)
    index: usize,
}

impl App {
    pub fn new(model_id: ModelId, context_window: u64) -> Self {
        Self {
            messages: Vec::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            is_streaming: false,
            input: String::new(),
            cursor: 0,
            scroll_offset: 0,
            model_id,
            stats: TokenStats::new(context_window),
            should_quit: false,
            status: None,
            active_tools: Vec::new(),
            streaming_tool_args: String::new(),
            visible_text_chars: 0,
            visible_thinking_chars: 0,
            throbber_state: ThrobberState::default(),
            tick_count: 0,
            thinking_level: ThinkingLevel::Off,
            thinking_display: ThinkingDisplay::default(),
            mode: AppMode::Normal,
            session_picker: None,
            slash_menu: None,
            slash_commands: Vec::new(),
            model_completions: Vec::new(),
            completions: Vec::new(),
            tab_state: None,
            has_unread: false,
            confirm_prompt: None,
            show_prompt_injection: false,
            show_cost: false,
            selected_message: None,
            search: SearchState::default(),
            image_render_areas: RefCell::new(Vec::new()),
            pending_images: Vec::new(),
            cwd: {
                let path = std::env::current_dir().unwrap_or_default();
                match std::env::var("HOME") {
                    Ok(home) => path
                        .strip_prefix(&home)
                        .map(|p| format!("~/{}", p.display()))
                        .unwrap_or_else(|_| path.display().to_string()),
                    Err(_) => path.display().to_string(),
                }
            },
            total_content_lines: Cell::new(0),
            visible_area_height: Cell::new(0),
            input_scroll: Cell::new(0),
            input_total_lines: Cell::new(0),
            input_visible_lines: Cell::new(0),
            input_area: Cell::new(Rect::default()),
            pane_info: None,
            background_alert: None,
        }
    }

    /// advance the spinner state (throttled to ~8fps from ~60fps frame rate)
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        if self.tick_count.is_multiple_of(8) {
            self.throbber_state.calc_next();
        }
        // typewriter: advance visible chars towards full buffer using exponential ease
        if self.is_streaming {
            let text_total = self.streaming_text.chars().count();
            if self.visible_text_chars < text_total {
                let remaining = text_total - self.visible_text_chars;
                self.visible_text_chars += remaining.div_ceil(2).max(1);
            }
            let think_total = self.streaming_thinking.chars().count();
            if self.visible_thinking_chars < think_total {
                let remaining = think_total - self.visible_thinking_chars;
                self.visible_thinking_chars += remaining.div_ceil(2).max(1);
            }
        }
    }

    /// whether the unread flash indicator is in the "on" phase
    /// cycles at ~1hz (30 ticks on, 30 off at ~60fps)
    pub fn unread_flash_on(&self) -> bool {
        self.tick_count % 60 < 30
    }

    /// whether the agent is currently active (streaming or executing tools)
    pub fn is_busy(&self) -> bool {
        self.is_streaming
            || self
                .active_tools
                .iter()
                .any(|t| t.status == ToolCallStatus::Running)
    }

    /// add a user message to the display
    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: text.into(),
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        self.scroll_offset = 0;
    }

    /// add a queued steering message (shown dimmed until processed)
    pub fn push_queued_message(&mut self, text: impl Into<String>) {
        self.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: text.into(),
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: true,
        });
        self.scroll_offset = 0;
    }

    /// start streaming a new assistant message
    pub fn start_streaming(&mut self) {
        self.is_streaming = true;
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.visible_text_chars = 0;
        self.visible_thinking_chars = 0;
        self.scroll_offset = 0;
    }

    /// append text delta to the current stream
    pub fn push_text_delta(&mut self, delta: &str) {
        self.streaming_text.push_str(delta);
    }

    /// append thinking delta to the current stream
    pub fn push_thinking_delta(&mut self, delta: &str) {
        self.streaming_thinking.push_str(delta);
    }

    /// visible portion of streaming text (typewriter effect)
    pub fn visible_streaming_text(&self) -> &str {
        char_prefix(&self.streaming_text, self.visible_text_chars)
    }

    /// visible portion of streaming thinking (typewriter effect)
    pub fn visible_streaming_thinking(&self) -> &str {
        char_prefix(&self.streaming_thinking, self.visible_thinking_chars)
    }

    /// accumulate streaming tool call arguments
    pub fn push_tool_args_delta(&mut self, delta: &str) {
        self.streaming_tool_args.push_str(delta);
    }

    /// mark a tool as being executed
    pub fn start_tool(&mut self, tool_call_id: &str, name: &str, summary: &str) {
        self.active_tools.push(ActiveToolState {
            tool_call_id: tool_call_id.to_string(),
            name: name.to_string(),
            summary: summary.to_string(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });
        self.streaming_tool_args.clear();
        // add to the last message's tool calls if we have one in progress
        if let Some(last) = self.messages.last_mut() {
            last.tool_calls.push(DisplayToolCall {
                name: name.to_string(),
                summary: summary.to_string(),
                status: ToolCallStatus::Running,
                output_preview: None,
                image_data: None,
            });
        }
    }

    /// mark a tool as done, with optional output preview
    pub fn end_tool(
        &mut self,
        tool_call_id: &str,
        name: &str,
        outcome: mush_ai::types::ToolOutcome,
        output: Option<&str>,
        image_data: Option<Vec<u8>>,
    ) {
        let status = if outcome.is_error() {
            ToolCallStatus::Error
        } else {
            ToolCallStatus::Done
        };
        // mark done in active_tools (panel persists until next turn)
        if let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| t.tool_call_id == tool_call_id)
        {
            tool.status = status.clone();
            tool.output = output.map(truncate_output);
            tool.live_output = None;
        }
        if let Some(last) = self.messages.last_mut()
            && let Some(tc) = last.tool_calls.iter_mut().rfind(|t| t.name == name)
        {
            tc.status = status;
            tc.output_preview = output.map(truncate_output);
            tc.image_data = image_data;
        }
    }

    /// update live output for an active tool
    pub fn push_tool_output(&mut self, tool_call_id: &str, output: &str) {
        if let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| t.tool_call_id == tool_call_id)
        {
            tool.live_output = Some(output.to_string());
        }
    }

    /// finish streaming, create the assistant message
    pub fn finish_streaming(&mut self, usage: Option<Usage>, cost: Option<f64>) {
        self.is_streaming = false;
        let thinking = if self.streaming_thinking.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.streaming_thinking))
        };
        let text = std::mem::take(&mut self.streaming_text);

        let assistant_msg = DisplayMessage {
            role: MessageRole::Assistant,
            content: text.trim_start_matches('\n').to_string(),
            tool_calls: vec![],
            thinking,
            thinking_expanded: self.thinking_display == ThinkingDisplay::Expanded,
            usage,
            cost,
            model_id: Some(self.model_id.clone()),
            queued: false,
        };

        // insert before any trailing queued (steering) messages so the
        // assistant reply appears above steering input in the message list
        let insert_pos = self
            .messages
            .iter()
            .rposition(|m| !m.queued)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.messages.insert(insert_pos, assistant_msg);

        if let Some(ref u) = usage {
            self.stats.update(u, cost);
        } else if let Some(c) = cost {
            self.stats.total_cost += c;
        }
        if self.scroll_offset > 0 {
            self.has_unread = true;
        }
    }

    /// insert a character at the cursor
    pub fn input_char(&mut self, c: char) {
        self.tab_state = None;
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.ensure_cursor_visible();
    }

    /// delete character before cursor
    pub fn input_backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.remove_images_in_range(prev, self.cursor);
            self.input.drain(prev..self.cursor);
            self.cursor = prev;
            self.ensure_cursor_visible();
        }
    }

    /// delete character at cursor
    pub fn input_delete(&mut self) {
        if self.cursor < self.input.len() {
            let next = self.input[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.input.len());
            self.remove_images_in_range(self.cursor, next);
            self.input.drain(self.cursor..next);
            self.ensure_cursor_visible();
        }
    }

    /// move cursor left
    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.ensure_cursor_visible();
        }
    }

    /// move cursor right
    pub fn cursor_right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor = self.input[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.input.len());
            self.ensure_cursor_visible();
        }
    }

    /// move cursor one word left
    pub fn cursor_word_left(&mut self) {
        self.cursor = word_boundary_left(&self.input, self.cursor);
        self.ensure_cursor_visible();
    }

    /// move cursor one word right
    pub fn cursor_word_right(&mut self) {
        self.cursor = word_boundary_right(&self.input, self.cursor);
        self.ensure_cursor_visible();
    }

    /// move cursor to start
    pub fn cursor_home(&mut self) {
        self.cursor = 0;
        self.ensure_cursor_visible();
    }

    /// move cursor to end
    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
        self.ensure_cursor_visible();
    }

    /// delete word before cursor
    pub fn delete_word_backward(&mut self) {
        let boundary = word_boundary_left(&self.input, self.cursor);
        self.remove_images_in_range(boundary, self.cursor);
        self.input.drain(boundary..self.cursor);
        self.cursor = boundary;
        self.ensure_cursor_visible();
    }

    /// delete word after cursor
    pub fn delete_word_forward(&mut self) {
        let boundary = word_boundary_right(&self.input, self.cursor);
        self.remove_images_in_range(self.cursor, boundary);
        self.input.drain(self.cursor..boundary);
        self.ensure_cursor_visible();
    }

    /// delete from cursor to end of line
    pub fn delete_to_end(&mut self) {
        self.remove_images_in_range(self.cursor, self.input.len());
        self.input.truncate(self.cursor);
        self.ensure_cursor_visible();
    }

    /// delete from cursor to start of line
    pub fn delete_to_start(&mut self) {
        self.remove_images_in_range(0, self.cursor);
        self.input.drain(..self.cursor);
        self.cursor = 0;
        self.ensure_cursor_visible();
    }

    /// cycle through tab completions for the current input
    pub fn tab_complete(&mut self) {
        if let Some(ref mut state) = self.tab_state {
            // already completing: cycle to next match
            state.index = (state.index + 1) % state.matches.len();
            let replacement = &state.matches[state.index];
            self.input = replacement.clone();
            self.cursor = self.input.len();
            self.ensure_cursor_visible();
            return;
        }

        // start a new completion
        let input = self.input.as_str();
        let matches: Vec<String> = if let Some(rest) = input.strip_prefix("/model ") {
            // complete model ids
            self.completions
                .iter()
                .filter(|c| !c.starts_with('/'))
                .filter(|c| c.starts_with(rest))
                .map(|c| format!("/model {c}"))
                .collect()
        } else if input.starts_with('/') {
            // complete slash commands
            self.completions
                .iter()
                .filter(|c| c.starts_with(input))
                .cloned()
                .collect()
        } else {
            return;
        };

        if matches.is_empty() {
            return;
        }

        let first = matches[0].clone();
        self.tab_state = Some(TabState { matches, index: 0 });
        self.input = first;
        self.cursor = self.input.len();
        self.ensure_cursor_visible();
    }

    /// open the slash command completion menu, filtering by current input
    pub fn open_slash_menu(&mut self) {
        let prefix = self.input.as_str();

        if let Some(rest) = prefix.strip_prefix("/model ") {
            let query = rest.to_lowercase();
            let model_matches: Vec<ModelCompletion> = self
                .model_completions
                .iter()
                .filter(|m| m.id.starts_with(rest) || m.name.to_lowercase().contains(&query))
                .cloned()
                .collect();

            if model_matches.is_empty() {
                return;
            }

            self.slash_menu = Some(SlashMenuState {
                matches: Vec::new(),
                model_matches,
                model_mode: true,
                selected: 0,
            });
            self.mode = AppMode::SlashComplete;
            return;
        }

        let matches: Vec<SlashCommand> = self
            .slash_commands
            .iter()
            .filter(|cmd| {
                let full = format!("/{}", cmd.name);
                full.starts_with(prefix)
            })
            .cloned()
            .collect();

        if matches.is_empty() {
            return;
        }

        self.slash_menu = Some(SlashMenuState {
            matches,
            model_matches: Vec::new(),
            model_mode: false,
            selected: 0,
        });
        self.mode = AppMode::SlashComplete;
    }

    /// update the slash menu filter based on current input
    pub fn update_slash_menu(&mut self) {
        if let Some(ref mut menu) = self.slash_menu {
            let prefix = self.input.as_str();

            if let Some(rest) = prefix.strip_prefix("/model ") {
                let query = rest.to_lowercase();
                menu.model_mode = true;
                menu.model_matches = self
                    .model_completions
                    .iter()
                    .filter(|m| m.id.starts_with(rest) || m.name.to_lowercase().contains(&query))
                    .cloned()
                    .collect();
                menu.matches.clear();
                menu.selected = menu
                    .selected
                    .min(menu.model_matches.len().saturating_sub(1));

                if menu.model_matches.is_empty() {
                    self.close_slash_menu();
                }
                return;
            }

            menu.model_mode = false;
            menu.matches = self
                .slash_commands
                .iter()
                .filter(|cmd| {
                    let full = format!("/{}", cmd.name);
                    full.starts_with(prefix)
                })
                .cloned()
                .collect();
            menu.model_matches.clear();
            menu.selected = menu.selected.min(menu.matches.len().saturating_sub(1));

            if menu.matches.is_empty() {
                self.close_slash_menu();
            }
        }
    }

    /// close the slash menu and return to normal mode
    pub fn close_slash_menu(&mut self) {
        self.slash_menu = None;
        self.mode = AppMode::Normal;
    }

    /// jump to bottom of conversation and clear unread indicator
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.has_unread = false;
    }

    /// update search matches based on current query
    pub fn update_search(&mut self) {
        let q = self.search.query.to_lowercase();
        self.search.matches = if q.is_empty() {
            Vec::new()
        } else {
            self.messages
                .iter()
                .enumerate()
                .filter(|(_, m)| m.content.to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .collect()
        };
        // clamp selection
        if self.search.matches.is_empty() {
            self.search.selected = 0;
        } else if self.search.selected >= self.search.matches.len() {
            self.search.selected = self.search.matches.len() - 1;
        }
    }

    /// return ghost completion suffix for inline hint (dimmed text after cursor).
    /// only shown when cursor is at end and no active tab cycle.
    pub fn ghost_text(&self) -> Option<&str> {
        // don't show ghost while actively cycling completions
        if self.tab_state.is_some() {
            return None;
        }
        // only when cursor is at the end
        if self.cursor != self.input.len() || self.input.is_empty() {
            return None;
        }
        let input = self.input.as_str();
        let candidate = if let Some(rest) = input.strip_prefix("/model ") {
            self.completions
                .iter()
                .filter(|c| !c.starts_with('/'))
                .find(|c| c.starts_with(rest))
                .map(|c| &c[rest.len()..])
        } else if input.starts_with('/') {
            self.completions
                .iter()
                .find(|c| c.starts_with(input) && c.len() > input.len())
                .map(|c| &c[input.len()..])
        } else {
            None
        };
        candidate.filter(|s| !s.is_empty())
    }

    /// clear all messages (for /clear command)
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.visible_text_chars = 0;
        self.visible_thinking_chars = 0;
        self.scroll_offset = 0;
        self.stats.reset();
        self.pending_images.clear();
    }

    /// remove pending images whose placeholders fall within input[start..end]
    fn remove_images_in_range(&mut self, start: usize, end: usize) {
        let range = &self.input[start..end];
        if !range.contains(IMAGE_PLACEHOLDER) {
            return;
        }
        // image index = number of placeholders before this one
        let prior = self.input[..start]
            .chars()
            .filter(|c| *c == IMAGE_PLACEHOLDER)
            .count();
        // count how many placeholders are in the range, remove in reverse
        let count = range.chars().filter(|c| *c == IMAGE_PLACEHOLDER).count();
        for i in (0..count).rev() {
            let idx = prior + i;
            if idx < self.pending_images.len() {
                self.pending_images.remove(idx);
            }
        }
    }

    /// push a system message to the display
    pub fn push_system_message(&mut self, text: impl Into<String>) {
        self.messages.push(DisplayMessage {
            role: MessageRole::System,
            content: text.into(),
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
    }

    /// toggle thinking text visibility for the last assistant message
    pub fn toggle_thinking_expanded(&mut self) {
        if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Assistant && m.thinking.is_some())
        {
            msg.thinking_expanded = !msg.thinking_expanded;
        }
    }

    /// cycle to the next thinking level
    pub fn cycle_thinking_level(&mut self) {
        self.thinking_level = match self.thinking_level {
            ThinkingLevel::Off => ThinkingLevel::Minimal,
            ThinkingLevel::Minimal => ThinkingLevel::Low,
            ThinkingLevel::Low => ThinkingLevel::Medium,
            ThinkingLevel::Medium => ThinkingLevel::High,
            ThinkingLevel::High => ThinkingLevel::Xhigh,
            ThinkingLevel::Xhigh => ThinkingLevel::Off,
        };
    }

    /// take the input text and reset (strips image placeholders)
    pub fn take_input(&mut self) -> String {
        self.cursor = 0;
        self.input_scroll.set(0);
        let input = std::mem::take(&mut self.input);
        input.replace(IMAGE_PLACEHOLDER, "")
    }

    /// add a clipboard image to pending attachments, inserting a
    /// placeholder at the cursor so it appears inline in the input
    pub fn add_image(&mut self, image: ClipboardImage) {
        let dimensions = image::load_from_memory(&image.bytes)
            .ok()
            .map(|img| (img.width(), img.height()));
        self.pending_images.push(PendingImage {
            data: image.bytes,
            mime_type: image.mime_type,
            dimensions,
        });
        self.input.insert(self.cursor, IMAGE_PLACEHOLDER);
        self.cursor += IMAGE_PLACEHOLDER.len_utf8();
        self.ensure_cursor_visible();
    }

    /// take pending images (clearing them from the app)
    pub fn take_images(&mut self) -> Vec<PendingImage> {
        std::mem::take(&mut self.pending_images)
    }

    /// remove the last pending image (and its placeholder in the input)
    pub fn remove_last_image(&mut self) {
        if self.pending_images.pop().is_some()
            && let Some(pos) = self.input.rfind(IMAGE_PLACEHOLDER)
        {
            let end = pos + IMAGE_PLACEHOLDER.len_utf8();
            self.input.drain(pos..end);
            if self.cursor > pos {
                self.cursor = self.cursor.saturating_sub(IMAGE_PLACEHOLDER.len_utf8());
            }
            self.ensure_cursor_visible();
        }
    }

    /// open the session picker with the given sessions
    pub fn open_session_picker(&mut self, sessions: Vec<SessionMeta>, cwd: String) {
        self.session_picker = Some(SessionPickerState {
            sessions,
            selected: 0,
            filter: String::new(),
            scope: SessionScope::ThisDir,
            cwd,
        });
        self.mode = AppMode::SessionPicker;
    }

    /// close the session picker
    pub fn close_session_picker(&mut self) {
        self.session_picker = None;
        self.mode = AppMode::Normal;
    }

    /// get the currently selected session id (if picker is open)
    pub fn selected_session(&self) -> Option<&SessionMeta> {
        let picker = self.session_picker.as_ref()?;
        let filtered = filtered_sessions(picker);
        filtered.get(picker.selected).copied()
    }

    /// ensure input scroll keeps the cursor visible
    pub fn ensure_cursor_visible(&self) {
        let content_width = self.input_area.get().width.saturating_sub(2);
        let visible_lines = self.input_visible_lines.get();
        if content_width == 0 || visible_lines == 0 {
            self.input_scroll.set(0);
            return;
        }

        let expanded =
            crate::widgets::input_box::expand_input(&self.input, self.cursor, &self.pending_images);
        let cursor_line = crate::widgets::input_box::cursor_visual_line(
            &expanded.text,
            expanded.cursor,
            content_width as usize,
        ) as u16;

        let total = expanded
            .text
            .split('\n')
            .enumerate()
            .map(|(i, line)| {
                let indent = if i == 0 { 2 } else { 0 };
                crate::widgets::input_box::word_wrap_segments(line, content_width as usize, indent)
                    .len() as u16
            })
            .sum::<u16>()
            .max(1);
        self.input_total_lines.set(total);

        let mut scroll = self.input_scroll.get();
        if cursor_line < scroll {
            scroll = cursor_line;
        } else {
            let bottom = scroll.saturating_add(visible_lines.saturating_sub(1));
            if cursor_line > bottom {
                scroll = cursor_line.saturating_sub(visible_lines.saturating_sub(1));
            }
        }

        let max_scroll = total.saturating_sub(visible_lines);
        self.input_scroll.set(scroll.min(max_scroll));
    }

    /// scroll the input viewport by delta lines
    pub fn scroll_input_by(&self, delta: i16) {
        let visible = self.input_visible_lines.get();
        let total = self.input_total_lines.get();
        let max_scroll = total.saturating_sub(visible);
        let current = self.input_scroll.get() as i16;
        let next = (current + delta).clamp(0, max_scroll as i16) as u16;
        self.input_scroll.set(next);
    }

    /// whether the mouse position is over the input box
    pub fn is_mouse_over_input(&self, column: u16, row: u16) -> bool {
        self.input_area
            .get()
            .contains(ratatui::layout::Position::new(column, row))
    }
}

/// get sessions matching the current filter and scope
#[must_use]
pub fn filtered_sessions(picker: &SessionPickerState) -> Vec<&SessionMeta> {
    let scope_filtered: Vec<&SessionMeta> = match picker.scope {
        SessionScope::ThisDir => picker
            .sessions
            .iter()
            .filter(|s| s.cwd == picker.cwd)
            .collect(),
        SessionScope::AllDirs => picker.sessions.iter().collect(),
    };

    if picker.filter.is_empty() {
        scope_filtered
    } else {
        let filter_lower = picker.filter.to_lowercase();
        scope_filtered
            .into_iter()
            .filter(|s| {
                s.title
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&filter_lower)
                    || s.id.contains(&filter_lower)
                    || s.cwd.to_lowercase().contains(&filter_lower)
            })
            .collect()
    }
}

/// find the byte offset of the previous word boundary
fn word_boundary_left(s: &str, cursor: usize) -> usize {
    let before = &s[..cursor];
    if before.is_empty() {
        return 0;
    }
    // skip trailing whitespace and punctuation (non-word chars)
    let skip_end = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric() || *c == '_')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if skip_end == 0 {
        return 0;
    }
    // now find start of the word
    before[..skip_end]
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// find the byte offset of the next word boundary
fn word_boundary_right(s: &str, cursor: usize) -> usize {
    let after = &s[cursor..];
    // skip current word chars, then skip whitespace
    let mut chars = after.char_indices();
    // skip word chars
    let word_end = chars
        .by_ref()
        .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    // skip whitespace/punctuation
    let past_ws = after[word_end..]
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric() || *c == '_')
        .map(|(i, _)| word_end + i)
        .unwrap_or(after.len());
    cursor + past_ws
}

/// max lines to show in tool output preview
const MAX_PREVIEW_LINES: usize = 12;
/// max chars per preview line
const MAX_PREVIEW_LINE_LEN: usize = 120;

fn truncate_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    let preview: Vec<String> = lines
        .into_iter()
        .take(MAX_PREVIEW_LINES)
        .map(|l| {
            if l.len() > MAX_PREVIEW_LINE_LEN {
                format!("{}...", &l[..MAX_PREVIEW_LINE_LEN])
            } else {
                l.to_string()
            }
        })
        .collect();
    let mut result = preview.join("\n");
    if total > MAX_PREVIEW_LINES {
        result.push_str(&format!("\n... ({} more lines)", total - MAX_PREVIEW_LINES));
    }
    result
}

/// return the first `n` chars of `s` as a str slice
fn char_prefix(s: &str, n: usize) -> &str {
    s.char_indices()
        .nth(n)
        .map_or(s, |(byte_pos, _)| &s[..byte_pos])
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::ToolOutcome;

    #[test]
    fn new_app_is_empty() {
        let app = App::new("test-model".into(), 200_000);
        assert!(app.messages.is_empty());
        assert!(!app.is_streaming);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn push_user_message() {
        let mut app = App::new("test".into(), 200_000);
        app.push_user_message("hello");
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::User);
        assert_eq!(app.messages[0].content, "hello");
    }

    #[test]
    fn streaming_lifecycle() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        assert!(app.is_streaming);

        app.push_text_delta("hello ");
        app.push_text_delta("world");
        assert_eq!(app.streaming_text, "hello world");

        app.finish_streaming(None, None);
        assert!(!app.is_streaming);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "hello world");
        assert!(app.streaming_text.is_empty());
    }

    #[test]
    fn streaming_with_thinking() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_thinking_delta("let me think...");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        assert_eq!(app.messages[0].thinking.as_deref(), Some("let me think..."));
        assert_eq!(app.messages[0].content, "answer");
    }

    #[test]
    fn char_prefix_slices_correctly() {
        assert_eq!(char_prefix("hello", 0), "");
        assert_eq!(char_prefix("hello", 3), "hel");
        assert_eq!(char_prefix("hello", 5), "hello");
        assert_eq!(char_prefix("hello", 100), "hello");
        // multi-byte chars
        assert_eq!(char_prefix("café", 3), "caf");
        assert_eq!(char_prefix("café", 4), "café");
    }

    #[test]
    fn typewriter_advances_with_ticks() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_text_delta("hello world");

        // before any tick, nothing visible
        assert_eq!(app.visible_streaming_text(), "");

        // one tick advances partway (exponential ease)
        app.tick();
        let visible = app.visible_streaming_text();
        assert!(!visible.is_empty());
        assert!(visible.len() < "hello world".len());

        // enough ticks catch up fully
        for _ in 0..20 {
            app.tick();
        }
        assert_eq!(app.visible_streaming_text(), "hello world");
    }

    #[test]
    fn typewriter_resets_on_new_stream() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_text_delta("first");
        for _ in 0..20 {
            app.tick();
        }
        assert_eq!(app.visible_streaming_text(), "first");

        // new stream resets
        app.start_streaming();
        assert_eq!(app.visible_streaming_text(), "");
    }

    #[test]
    fn tool_lifecycle() {
        let mut app = App::new("test".into(), 200_000);
        app.push_user_message("do something");
        // simulate assistant message already pushed by finish_streaming
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });

        app.start_tool("tc_1", "bash", "ls -la");
        assert_eq!(app.active_tools.len(), 1);
        assert_eq!(app.active_tools[0].name, "bash");
        assert_eq!(app.messages.last().unwrap().tool_calls.len(), 1);
        assert_eq!(
            app.messages.last().unwrap().tool_calls[0].status,
            ToolCallStatus::Running
        );

        app.end_tool(
            "tc_1",
            "bash",
            ToolOutcome::Success,
            Some("file1.txt\nfile2.txt"),
            None,
        );
        // tool stays in active_tools as done (panel persists)
        assert_eq!(app.active_tools.len(), 1);
        assert_eq!(app.active_tools[0].status, ToolCallStatus::Done);
        assert_eq!(
            app.messages.last().unwrap().tool_calls[0].status,
            ToolCallStatus::Done
        );

        // tools persist through streaming turns (cleared on new user submit)
        app.start_streaming();
        assert_eq!(app.active_tools.len(), 1);
        assert_eq!(app.active_tools[0].status, ToolCallStatus::Done);

        // explicitly clearing simulates user submitting new prompt
        app.active_tools.clear();
        assert!(app.active_tools.is_empty());
    }

    #[test]
    fn ensure_cursor_visible_recomputes_total_when_stale() {
        let mut app = App::new("test".into(), 200_000);
        app.input_area.set(Rect::new(0, 0, 20, 8));
        app.input_visible_lines.set(2);
        app.input_total_lines.set(2); // stale from previous render
        app.input = "one\ntwo\nthree\nfour\nfive".into();
        app.cursor = app.input.len();

        app.ensure_cursor_visible();

        assert!(app.input_total_lines.get() >= 5);
        assert!(app.input_scroll.get() > 0);
    }

    #[test]
    fn input_editing() {
        let mut app = App::new("test".into(), 200_000);
        app.input_char('h');
        app.input_char('i');
        assert_eq!(app.input, "hi");
        assert_eq!(app.cursor, 2);

        app.cursor_left();
        assert_eq!(app.cursor, 1);
        app.input_char('!');
        assert_eq!(app.input, "h!i");

        app.cursor_home();
        assert_eq!(app.cursor, 0);
        app.cursor_end();
        assert_eq!(app.cursor, 3);

        app.input_backspace();
        assert_eq!(app.input, "h!");
    }

    #[test]
    fn input_delete() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "abc".into();
        app.cursor = 1;
        app.input_delete();
        assert_eq!(app.input, "ac");
    }

    #[test]
    fn take_input_resets() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello".into();
        app.cursor = 3;
        let text = app.take_input();
        assert_eq!(text, "hello");
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn cost_accumulates() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_text_delta("a");
        app.finish_streaming(
            Some(Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
            Some(0.005),
        );

        app.start_streaming();
        app.push_text_delta("b");
        app.finish_streaming(
            Some(Usage {
                input_tokens: 200,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
            Some(0.01),
        );

        assert!((app.stats.total_cost - 0.015).abs() < f64::EPSILON);
        assert_eq!(app.stats.total_tokens, 450);
        assert_eq!(app.stats.input_tokens, 300);
        assert_eq!(app.stats.output_tokens, 150);
        assert_eq!(app.stats.cache_read_tokens, 0);
        assert_eq!(app.stats.cache_write_tokens, 0);
        assert_eq!(app.stats.context_tokens, 200);
    }

    #[test]
    fn truncate_short_output() {
        let output = truncate_output("hello\nworld");
        assert_eq!(output, "hello\nworld");
    }

    #[test]
    fn truncate_long_output() {
        let lines = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = truncate_output(&lines);
        assert!(output.contains("line 0"));
        assert!(output.contains("line 11"));
        assert!(output.contains("8 more lines"));
        assert!(!output.contains("line 12\n"));
    }

    #[test]
    fn truncate_long_line() {
        let long = "x".repeat(200);
        let output = truncate_output(&long);
        assert!(output.len() < 200);
        assert!(output.ends_with("..."));
    }

    #[test]
    fn tool_output_stored() {
        let mut app = App::new("test".into(), 200_000);
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        app.start_tool("tc_1", "read", "src/main.rs");
        app.end_tool(
            "tc_1",
            "read",
            ToolOutcome::Success,
            Some("fn main() {}\n"),
            None,
        );
        let tc = &app.messages.last().unwrap().tool_calls[0];
        assert!(tc.output_preview.is_some());
        assert!(tc.output_preview.as_ref().unwrap().contains("fn main()"));
    }

    #[test]
    fn toggle_thinking() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_thinking_delta("deep thoughts");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        // starts expanded (default ThinkingDisplay::Expanded)
        assert!(app.messages[0].thinking_expanded);

        app.toggle_thinking_expanded();
        assert!(!app.messages[0].thinking_expanded);

        app.toggle_thinking_expanded();
        assert!(app.messages[0].thinking_expanded);
    }

    #[test]
    fn toggle_thinking_targets_last_assistant() {
        let mut app = App::new("test".into(), 200_000);

        // first assistant with thinking
        app.start_streaming();
        app.push_thinking_delta("old thoughts");
        app.push_text_delta("first");
        app.finish_streaming(None, None);

        // second assistant with thinking
        app.start_streaming();
        app.push_thinking_delta("new thoughts");
        app.push_text_delta("second");
        app.finish_streaming(None, None);

        app.toggle_thinking_expanded();
        // should toggle the latest one (was expanded, now collapsed)
        assert!(app.messages[0].thinking_expanded);
        assert!(!app.messages[1].thinking_expanded);
    }

    #[test]
    fn thinking_display_collapse_starts_collapsed() {
        let mut app = App::new("test".into(), 200_000);
        app.thinking_display = ThinkingDisplay::Collapse;
        app.start_streaming();
        app.push_thinking_delta("deep thoughts");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        assert!(!app.messages[0].thinking_expanded);
    }

    #[test]
    fn thinking_display_hidden_starts_collapsed() {
        let mut app = App::new("test".into(), 200_000);
        app.thinking_display = ThinkingDisplay::Hidden;
        app.start_streaming();
        app.push_thinking_delta("deep thoughts");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        assert!(!app.messages[0].thinking_expanded);
    }

    #[test]
    fn multi_line_input() {
        let mut app = App::new("test".into(), 200_000);
        app.input_char('a');
        app.input_char('\n');
        app.input_char('b');
        assert_eq!(app.input, "a\nb");
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn session_picker_open_close() {
        let mut app = App::new("test".into(), 200_000);
        assert_eq!(app.mode, AppMode::Normal);

        let sessions = vec![SessionMeta {
            id: mush_session::SessionId::from("abc"),
            title: Some("test session".into()),
            model_id: "claude-sonnet".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 5,
            cwd: "/tmp".into(),
        }];

        app.open_session_picker(sessions, "/tmp".into());
        assert_eq!(app.mode, AppMode::SessionPicker);
        assert!(app.session_picker.is_some());

        app.close_session_picker();
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.session_picker.is_none());
    }

    #[test]
    fn session_picker_filter() {
        let sessions = vec![
            SessionMeta {
                id: mush_session::SessionId::from("a"),
                title: Some("rust project".into()),
                model_id: "m".into(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                message_count: 1,
                cwd: "/tmp".into(),
            },
            SessionMeta {
                id: mush_session::SessionId::from("b"),
                title: Some("python script".into()),
                model_id: "m".into(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                message_count: 2,
                cwd: "/tmp".into(),
            },
        ];

        let mut app = App::new("test".into(), 200_000);
        app.open_session_picker(sessions, "/tmp".into());

        let picker = app.session_picker.as_mut().unwrap();
        picker.filter = "rust".into();
        let filtered = filtered_sessions(picker);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title.as_deref(), Some("rust project"));
    }

    #[test]
    fn session_picker_scope_filter() {
        let sessions = vec![
            SessionMeta {
                id: mush_session::SessionId::from("a"),
                title: Some("local session".into()),
                model_id: "m".into(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                message_count: 1,
                cwd: "/home/user/project".into(),
            },
            SessionMeta {
                id: mush_session::SessionId::from("b"),
                title: Some("other session".into()),
                model_id: "m".into(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                message_count: 2,
                cwd: "/home/user/other".into(),
            },
        ];

        let mut app = App::new("test".into(), 200_000);
        app.open_session_picker(sessions, "/home/user/project".into());

        // this dir: only the matching session
        let picker = app.session_picker.as_ref().unwrap();
        assert_eq!(picker.scope, SessionScope::ThisDir);
        let filtered = filtered_sessions(picker);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title.as_deref(), Some("local session"));

        // all dirs: both sessions
        let picker = app.session_picker.as_mut().unwrap();
        picker.scope = SessionScope::AllDirs;
        let filtered = filtered_sessions(picker);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn streaming_tool_args_accumulate() {
        let mut app = App::new("test".into(), 200_000);
        app.push_tool_args_delta("{\"path\":");
        app.push_tool_args_delta("\"src/");
        assert_eq!(app.streaming_tool_args, "{\"path\":\"src/");

        // start_tool clears the buffer
        app.start_tool("tc_1", "read", "src/main.rs");
        assert!(app.streaming_tool_args.is_empty());
    }

    #[test]
    fn word_boundary_left_skips_punctuation() {
        // cursor after "hello." should delete back to start of "hello"
        assert_eq!(word_boundary_left("hello.", 6), 0);
        assert_eq!(word_boundary_left("foo hello.", 10), 4);
        assert_eq!(word_boundary_left("hello world.", 12), 6);
    }

    #[test]
    fn word_boundary_left_skips_asterisks() {
        assert_eq!(word_boundary_left("hello**", 7), 0);
        assert_eq!(word_boundary_left("one two**", 9), 4);
    }

    #[test]
    fn word_boundary_left_normal_words() {
        assert_eq!(word_boundary_left("hello world", 11), 6);
        assert_eq!(word_boundary_left("hello world  ", 13), 6);
        assert_eq!(word_boundary_left("hello", 5), 0);
    }

    #[test]
    fn word_boundary_left_all_punctuation() {
        assert_eq!(word_boundary_left("...", 3), 0);
    }

    #[test]
    fn steering_message_ordered_after_assistant() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_text_delta("assistant reply");
        // user sends steering while streaming
        app.push_queued_message("steer");
        // assistant finishes
        app.finish_streaming(None, None);

        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[0].role, MessageRole::Assistant);
        assert_eq!(app.messages[0].content, "assistant reply");
        assert_eq!(app.messages[1].role, MessageRole::User);
        assert_eq!(app.messages[1].content, "steer");
    }
}
