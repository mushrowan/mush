//! app state and event loop
//!
//! the app holds all TUI state: messages being displayed, current input,
//! streaming status, and scroll position.

use std::cell::RefCell;

use mush_ai::types::*;
use mush_session::SessionMeta;
use ratatui::layout::Rect;
use throbber_widgets_tui::ThrobberState;

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
}

/// which UI mode the app is in
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    SessionPicker,
    /// waiting for user to confirm a tool call (y/n)
    ToolConfirm,
    /// scroll mode: j/k scroll, y copies message, esc exits
    Scroll,
    /// search mode: type to filter messages, enter to jump
    Search,
}

/// state for the session picker overlay
#[derive(Debug, Clone)]
pub struct SessionPickerState {
    pub sessions: Vec<SessionMeta>,
    pub selected: usize,
    pub filter: String,
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
    /// total cost so far
    pub total_cost: f64,
    /// total tokens used (cumulative across all API calls)
    pub total_tokens: u64,
    /// cumulative uncached input tokens
    pub total_input_tokens: u64,
    /// cumulative output tokens
    pub total_output_tokens: u64,
    /// cumulative cache-read tokens
    pub total_cache_read_tokens: u64,
    /// cumulative cache-write tokens
    pub total_cache_write_tokens: u64,
    /// last call's input tokens (actual context size)
    pub context_tokens: u64,
    /// model's context window size
    pub context_window: u64,
    /// whether we should quit
    pub should_quit: bool,
    /// status message (bottom bar)
    pub status: Option<String>,
    /// currently executing tools (for side-by-side panel display)
    pub active_tools: Vec<ActiveToolState>,
    /// latest live output line from running tools
    pub tool_output_live: Option<String>,
    /// tool args streaming in (partial JSON from ToolCallDelta)
    pub streaming_tool_args: String,
    /// spinner state for animations
    pub throbber_state: ThrobberState,
    /// frame counter for throttling spinner speed
    tick_count: u8,
    /// current thinking level
    pub thinking_level: ThinkingLevel,
    /// which UI mode we're in
    pub mode: AppMode,
    /// session picker state (when mode == SessionPicker)
    pub session_picker: Option<SessionPickerState>,
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
    /// selected message index in scroll mode (for copy)
    pub selected_message: Option<usize>,
    /// search state
    pub search: SearchState,
    /// image render positions (populated by MessageList during render)
    pub image_render_areas: RefCell<Vec<ImageRenderArea>>,
}

/// position computed during render for inline image overlay
#[derive(Debug, Clone)]
pub struct ImageRenderArea {
    pub msg_idx: usize,
    pub tc_idx: usize,
    pub area: Rect,
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
            total_cost: 0.0,
            total_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            context_tokens: 0,
            context_window,
            should_quit: false,
            status: None,
            active_tools: Vec::new(),
            tool_output_live: None,
            streaming_tool_args: String::new(),
            throbber_state: ThrobberState::default(),
            tick_count: 0,
            thinking_level: ThinkingLevel::Off,
            mode: AppMode::Normal,
            session_picker: None,
            completions: Vec::new(),
            tab_state: None,
            has_unread: false,
            confirm_prompt: None,
            show_prompt_injection: false,
            selected_message: None,
            search: SearchState::default(),
            image_render_areas: RefCell::new(Vec::new()),
        }
    }

    /// advance the spinner state (throttled to ~8fps from ~60fps frame rate)
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        if self.tick_count.is_multiple_of(8) {
            self.throbber_state.calc_next();
        }
    }

    /// add a user message to the display
    pub fn push_user_message(&mut self, text: String) {
        self.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: text,
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
        });
        self.scroll_offset = 0;
    }

    /// start streaming a new assistant message
    pub fn start_streaming(&mut self) {
        self.is_streaming = true;
        self.streaming_text.clear();
        self.streaming_thinking.clear();
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
        is_error: bool,
        output: Option<&str>,
        image_data: Option<Vec<u8>>,
    ) {
        self.active_tools.retain(|t| t.tool_call_id != tool_call_id);
        if let Some(last) = self.messages.last_mut()
            && let Some(tc) = last.tool_calls.iter_mut().rfind(|t| t.name == name)
        {
            tc.status = if is_error {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Done
            };
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

        self.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: text.trim_start_matches('\n').to_string(),
            tool_calls: vec![],
            thinking,
            thinking_expanded: false,
            usage,
            cost,
            model_id: Some(self.model_id.clone()),
        });

        if let Some(c) = cost {
            self.total_cost += c;
        }
        if let Some(ref u) = usage {
            self.total_tokens += u.total_tokens();
            self.total_input_tokens += u.input_tokens;
            self.total_output_tokens += u.output_tokens;
            self.total_cache_read_tokens += u.cache_read_tokens;
            self.total_cache_write_tokens += u.cache_write_tokens;
            self.context_tokens = u.total_input_tokens();
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
    }

    /// delete character before cursor
    pub fn input_backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.drain(prev..self.cursor);
            self.cursor = prev;
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
            self.input.drain(self.cursor..next);
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
        }
    }

    /// move cursor one word left
    pub fn cursor_word_left(&mut self) {
        self.cursor = word_boundary_left(&self.input, self.cursor);
    }

    /// move cursor one word right
    pub fn cursor_word_right(&mut self) {
        self.cursor = word_boundary_right(&self.input, self.cursor);
    }

    /// move cursor to start
    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// move cursor to end
    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// delete word before cursor
    pub fn delete_word_backward(&mut self) {
        let boundary = word_boundary_left(&self.input, self.cursor);
        self.input.drain(boundary..self.cursor);
        self.cursor = boundary;
    }

    /// delete word after cursor
    pub fn delete_word_forward(&mut self) {
        let boundary = word_boundary_right(&self.input, self.cursor);
        self.input.drain(self.cursor..boundary);
    }

    /// delete from cursor to end of line
    pub fn delete_to_end(&mut self) {
        self.input.truncate(self.cursor);
    }

    /// delete from cursor to start of line
    pub fn delete_to_start(&mut self) {
        self.input.drain(..self.cursor);
        self.cursor = 0;
    }

    /// cycle through tab completions for the current input
    pub fn tab_complete(&mut self) {
        if let Some(ref mut state) = self.tab_state {
            // already completing: cycle to next match
            state.index = (state.index + 1) % state.matches.len();
            let replacement = &state.matches[state.index];
            self.input = replacement.clone();
            self.cursor = self.input.len();
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
        self.scroll_offset = 0;
        self.total_cost = 0.0;
        self.total_tokens = 0;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.total_cache_read_tokens = 0;
        self.total_cache_write_tokens = 0;
        self.context_tokens = 0;
    }

    /// push a system message to the display
    pub fn push_system_message(&mut self, text: String) {
        self.messages.push(DisplayMessage {
            role: MessageRole::System,
            content: text,
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
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

    /// take the input text and reset
    pub fn take_input(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    /// open the session picker with the given sessions
    pub fn open_session_picker(&mut self, sessions: Vec<SessionMeta>) {
        self.session_picker = Some(SessionPickerState {
            sessions,
            selected: 0,
            filter: String::new(),
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
}

/// get sessions matching the current filter
pub fn filtered_sessions(picker: &SessionPickerState) -> Vec<&SessionMeta> {
    if picker.filter.is_empty() {
        picker.sessions.iter().collect()
    } else {
        let filter_lower = picker.filter.to_lowercase();
        picker
            .sessions
            .iter()
            .filter(|s| {
                s.title
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&filter_lower)
                    || s.id.contains(&filter_lower)
            })
            .collect()
    }
}

/// find the byte offset of the previous word boundary
fn word_boundary_left(s: &str, cursor: usize) -> usize {
    let before = &s[..cursor];
    // skip whitespace/punctuation, then skip word chars
    let trimmed = before.trim_end();
    if trimmed.is_empty() {
        return 0;
    }
    let end = trimmed.len();
    // find start of current word
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
        .min(end)
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
const MAX_PREVIEW_LINES: usize = 5;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        app.push_user_message("hello".into());
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
    fn tool_lifecycle() {
        let mut app = App::new("test".into(), 200_000);
        app.push_user_message("do something".into());
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
        });

        app.start_tool("tc_1", "bash", "ls -la");
        assert_eq!(app.active_tools.len(), 1);
        assert_eq!(app.active_tools[0].name, "bash");
        assert_eq!(app.messages.last().unwrap().tool_calls.len(), 1);
        assert_eq!(
            app.messages.last().unwrap().tool_calls[0].status,
            ToolCallStatus::Running
        );

        app.end_tool("tc_1", "bash", false, Some("file1.txt\nfile2.txt"), None);
        assert!(app.active_tools.is_empty());
        assert_eq!(
            app.messages.last().unwrap().tool_calls[0].status,
            ToolCallStatus::Done
        );
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

        assert!((app.total_cost - 0.015).abs() < f64::EPSILON);
        assert_eq!(app.total_tokens, 450);
        assert_eq!(app.total_input_tokens, 300);
        assert_eq!(app.total_output_tokens, 150);
        assert_eq!(app.total_cache_read_tokens, 0);
        assert_eq!(app.total_cache_write_tokens, 0);
        assert_eq!(app.context_tokens, 200);
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
        assert!(output.contains("line 4"));
        assert!(output.contains("15 more lines"));
        assert!(!output.contains("line 5\n"));
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
        });
        app.start_tool("tc_1", "read", "src/main.rs");
        app.end_tool("tc_1", "read", false, Some("fn main() {}\n"), None);
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

        // starts collapsed
        assert!(!app.messages[0].thinking_expanded);

        app.toggle_thinking_expanded();
        assert!(app.messages[0].thinking_expanded);

        app.toggle_thinking_expanded();
        assert!(!app.messages[0].thinking_expanded);
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
        // should toggle the latest one
        assert!(!app.messages[0].thinking_expanded);
        assert!(app.messages[1].thinking_expanded);
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

        app.open_session_picker(sessions);
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
        app.open_session_picker(sessions);

        let picker = app.session_picker.as_mut().unwrap();
        picker.filter = "rust".into();
        let filtered = filtered_sessions(picker);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title.as_deref(), Some("rust project"));
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
}
