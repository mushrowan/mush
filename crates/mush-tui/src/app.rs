//! app state and event loop
//!
//! the app holds all TUI state: messages being displayed, current input,
//! streaming status, and scroll position.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use mush_ai::types::*;
use mush_session::SessionMeta;
use ratatui::layout::Rect;
use throbber_widgets_tui::ThrobberState;

use crate::clipboard::ClipboardImage;

/// determine cache TTL in seconds from provider and retention settings.
/// returns 0 if caching is disabled or provider doesn't support it
pub fn cache_ttl_secs(provider: &Provider, retention: Option<&CacheRetention>) -> u16 {
    match provider {
        Provider::Anthropic => match retention.copied().unwrap_or(CacheRetention::Short) {
            CacheRetention::None => 0,
            CacheRetention::Short => 300, // 5 minutes
            CacheRetention::Long => 3600, // 1 hour
        },
        // openai: automatic caching, ~5-10 min, use 5 as conservative estimate
        // openrouter: passes through to underlying provider, assume anthropic-like
        _ => 300,
    }
}

/// tracks prompt cache warmth with countdown and notification flags
#[derive(Debug, Clone)]
pub struct CacheTimer {
    /// cache TTL in seconds (determined from provider/retention config)
    pub ttl_secs: u16,
    /// when the cache was last active (read or write)
    last_active: Option<Instant>,
    /// whether we already sent a "cache expiring soon" notification
    pub warn_sent: bool,
    /// whether we already sent a "cache expired" notification
    pub expired_sent: bool,
}

impl CacheTimer {
    #[must_use]
    pub fn new(ttl_secs: u16) -> Self {
        Self {
            ttl_secs,
            last_active: None,
            warn_sent: false,
            expired_sent: false,
        }
    }

    /// refresh the cache warmth timer (call when cache_read or cache_write > 0)
    pub fn refresh(&mut self) {
        self.last_active = Some(Instant::now());
        self.warn_sent = false;
        self.expired_sent = false;
    }

    /// seconds remaining before cache expires, None if no active cache
    #[must_use]
    pub fn remaining_secs(&self) -> Option<u16> {
        let elapsed = self.last_active?.elapsed().as_secs() as u16;
        if elapsed >= self.ttl_secs {
            Some(0)
        } else {
            Some(self.ttl_secs - elapsed)
        }
    }

    /// seconds since last cache activity, None if never active
    #[must_use]
    pub fn elapsed_secs(&self) -> Option<u64> {
        self.last_active.map(|t| t.elapsed().as_secs())
    }
}

/// tracks the in-progress assistant response (text, thinking, tool args)
#[derive(Debug, Clone)]
pub struct StreamingState {
    /// current text being streamed in
    pub text: String,
    /// current thinking being streamed in
    pub thinking: String,
    /// whether we're currently streaming a response
    pub active: bool,
    /// tool args streaming in (partial JSON from ToolCallDelta)
    pub tool_args: String,
    /// chars of text currently visible (typewriter effect)
    visible_text_chars: usize,
    /// chars of thinking currently visible (typewriter effect)
    visible_thinking_chars: usize,
}

impl StreamingState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            active: false,
            tool_args: String::new(),
            visible_text_chars: 0,
            visible_thinking_chars: 0,
        }
    }

    /// begin streaming a new assistant message
    pub fn start(&mut self) {
        self.active = true;
        self.text.clear();
        self.thinking.clear();
        self.tool_args.clear();
        self.visible_text_chars = 0;
        self.visible_thinking_chars = 0;
    }

    /// visible portion of streaming text (typewriter effect)
    #[must_use]
    pub fn visible_text(&self) -> &str {
        char_prefix(&self.text, self.visible_text_chars)
    }

    /// visible portion of streaming thinking (typewriter effect)
    #[must_use]
    pub fn visible_thinking(&self) -> &str {
        char_prefix(&self.thinking, self.visible_thinking_chars)
    }

    /// advance typewriter: move visible chars towards full buffer
    pub fn advance_typewriter(&mut self) {
        let text_total = self.text.chars().count();
        if self.visible_text_chars < text_total {
            let remaining = text_total - self.visible_text_chars;
            self.visible_text_chars += remaining.div_ceil(2).max(1);
        }
        let think_total = self.thinking.chars().count();
        if self.visible_thinking_chars < think_total {
            let remaining = think_total - self.visible_thinking_chars;
            self.visible_thinking_chars += remaining.div_ceil(2).max(1);
        }
    }

    /// finish streaming: take the text and thinking, deactivate
    pub fn take(&mut self) -> (String, Option<String>) {
        self.active = false;
        let thinking = if self.thinking.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.thinking))
        };
        let text = std::mem::take(&mut self.text);
        self.tool_args.clear();
        self.visible_text_chars = 0;
        self.visible_thinking_chars = 0;
        (text, thinking)
    }
}

impl Default for StreamingState {
    fn default() -> Self {
        Self::new()
    }
}

/// an image attached to the next user message (not yet sent)
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub data: Vec<u8>,
    pub mime_type: ImageMimeType,
    /// image dimensions (width, height) if decoded
    pub dimensions: Option<(u32, u32)>,
}

/// user input buffer with cursor, images, and scroll state
#[derive(Debug)]
pub struct InputBuffer {
    /// the input text
    pub text: String,
    /// cursor byte position in text
    pub cursor: usize,
    /// images attached to the next user message (not yet sent)
    pub images: Vec<PendingImage>,
    /// current input scroll offset (lines from top)
    pub scroll: Cell<u16>,
    /// total wrapped input lines (set during render by InputBox)
    pub total_lines: Cell<u16>,
    /// visible input lines (set during render by InputBox)
    pub visible_lines: Cell<u16>,
    /// latest input area rect (set during render by Ui)
    pub area: Cell<Rect>,
    pub(crate) layout_cache: RefCell<Option<crate::widgets::input_box::InputLayoutCache>>,
    #[cfg(test)]
    pub(crate) layout_builds: Cell<u32>,
}

impl InputBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            images: Vec::new(),
            scroll: Cell::new(0),
            total_lines: Cell::new(0),
            visible_lines: Cell::new(0),
            area: Cell::new(Rect::default()),
            layout_cache: RefCell::new(None),
            #[cfg(test)]
            layout_builds: Cell::new(0),
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.ensure_cursor_visible();
    }

    /// insert a string at the cursor (used for bracketed paste)
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.ensure_cursor_visible();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.remove_images_in_range(prev, self.cursor);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
            self.ensure_cursor_visible();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.remove_images_in_range(self.cursor, next);
            self.text.drain(self.cursor..next);
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_word_left(&mut self) {
        self.cursor = word_boundary_left(&self.text, self.cursor);
        self.ensure_cursor_visible();
    }

    pub fn cursor_word_right(&mut self) {
        self.cursor = word_boundary_right(&self.text, self.cursor);
        self.ensure_cursor_visible();
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
        self.ensure_cursor_visible();
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.text.len();
        self.ensure_cursor_visible();
    }

    pub fn delete_word_backward(&mut self) {
        let boundary = word_boundary_left(&self.text, self.cursor);
        self.remove_images_in_range(boundary, self.cursor);
        self.text.drain(boundary..self.cursor);
        self.cursor = boundary;
        self.ensure_cursor_visible();
    }

    pub fn delete_word_forward(&mut self) {
        let boundary = word_boundary_right(&self.text, self.cursor);
        self.remove_images_in_range(self.cursor, boundary);
        self.text.drain(self.cursor..boundary);
        self.ensure_cursor_visible();
    }

    pub fn delete_to_end(&mut self) {
        self.remove_images_in_range(self.cursor, self.text.len());
        self.text.truncate(self.cursor);
        self.ensure_cursor_visible();
    }

    pub fn delete_to_start(&mut self) {
        self.remove_images_in_range(0, self.cursor);
        self.text.drain(..self.cursor);
        self.cursor = 0;
        self.ensure_cursor_visible();
    }

    /// take the input text, clearing the buffer and returning text without
    /// image placeholders
    pub fn take_text(&mut self) -> String {
        self.cursor = 0;
        self.scroll.set(0);
        let input = std::mem::take(&mut self.text);
        input.replace(IMAGE_PLACEHOLDER, "")
    }

    /// take pending images (clearing them from the buffer)
    pub fn take_images(&mut self) -> Vec<PendingImage> {
        std::mem::take(&mut self.images)
    }

    /// add a clipboard image, inserting a placeholder at the cursor
    pub fn add_image(&mut self, image: ClipboardImage) {
        let dimensions = image::load_from_memory(&image.bytes)
            .ok()
            .map(|img| (img.width(), img.height()));
        self.images.push(PendingImage {
            data: image.bytes,
            mime_type: image.mime_type,
            dimensions,
        });
        self.text.insert(self.cursor, IMAGE_PLACEHOLDER);
        self.cursor += IMAGE_PLACEHOLDER.len_utf8();
        self.ensure_cursor_visible();
    }

    /// remove the last pending image (and its placeholder in the input)
    pub fn remove_last_image(&mut self) {
        if self.images.pop().is_some()
            && let Some(pos) = self.text.rfind(IMAGE_PLACEHOLDER)
        {
            let end = pos + IMAGE_PLACEHOLDER.len_utf8();
            self.text.drain(pos..end);
            if self.cursor > pos {
                self.cursor = self.cursor.saturating_sub(IMAGE_PLACEHOLDER.len_utf8());
            }
            self.ensure_cursor_visible();
        }
    }

    /// ensure input scroll keeps the cursor visible
    pub fn ensure_cursor_visible(&self) {
        let content_width = self.area.get().width.saturating_sub(2);
        let visible = self.visible_lines.get();
        if content_width == 0 || visible == 0 {
            self.scroll.set(0);
            return;
        }

        let layout = self.layout(content_width as usize);
        let cursor_line = layout.cursor_visual_line as u16;
        let total = layout.total_lines;
        self.total_lines.set(total);

        let mut current_scroll = self.scroll.get();
        if cursor_line < current_scroll {
            current_scroll = cursor_line;
        } else {
            let bottom = current_scroll.saturating_add(visible.saturating_sub(1));
            if cursor_line > bottom {
                current_scroll = cursor_line.saturating_sub(visible.saturating_sub(1));
            }
        }

        let max_scroll = total.saturating_sub(visible);
        self.scroll.set(current_scroll.min(max_scroll));
    }

    /// scroll the input viewport by delta lines
    pub fn scroll_by(&self, delta: i16) {
        let visible = self.visible_lines.get();
        let total = self.total_lines.get();
        let max_scroll = total.saturating_sub(visible);
        let current = self.scroll.get() as i16;
        let next = (current + delta).clamp(0, max_scroll as i16) as u16;
        self.scroll.set(next);
    }

    /// whether the mouse position is over the input box
    pub fn is_mouse_over(&self, column: u16, row: u16) -> bool {
        self.area
            .get()
            .contains(ratatui::layout::Position::new(column, row))
    }

    /// remove pending images whose placeholders fall within text[start..end]
    fn remove_images_in_range(&mut self, start: usize, end: usize) {
        let range = &self.text[start..end];
        if !range.contains(IMAGE_PLACEHOLDER) {
            return;
        }
        let prior = self.text[..start]
            .chars()
            .filter(|c| *c == IMAGE_PLACEHOLDER)
            .count();
        let count = range.chars().filter(|c| *c == IMAGE_PLACEHOLDER).count();
        for i in (0..count).rev() {
            let idx = prior + i;
            if idx < self.images.len() {
                self.images.remove(idx);
            }
        }
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// object replacement character, marks image positions in input text.
/// each occurrence maps to the Nth entry in pending_images (by order)
pub const IMAGE_PLACEHOLDER: char = '\u{FFFC}';

/// tick() runs at ~60fps, divide by this to get spinner update rate (~8fps)
const TICK_DIVISOR: u8 = 8;

/// ticks in one full unread-flash cycle (~1 second at 60fps)
const UNREAD_FLASH_CYCLE: u8 = 60;

/// ticks the flash indicator stays "on" within each cycle
const UNREAD_FLASH_ON: u8 = 30;

/// seconds before cache expiry to trigger a warning notification
pub const CACHE_WARN_SECS: u16 = 60;

/// seconds after cache expires to keep showing "cold" in status bar
pub const CACHE_COLD_DISPLAY_SECS: u64 = 30;

/// events that flow between the TUI and the agent
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// user submitted a prompt
    UserSubmit {
        text: String,
    },
    /// user executed a slash command
    SlashCommand {
        action: crate::slash::SlashAction,
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
    /// alt+k: edit a queued steering message
    EditSteering,
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

/// what j/k navigates in scroll mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollUnit {
    /// navigate between messages
    Message,
    /// navigate between fenced code blocks
    #[default]
    Block,
}

/// a fenced code block extracted from conversation messages
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    /// index into app.messages
    pub msg_idx: usize,
    /// raw content (no fences, no indent)
    pub content: String,
    /// language tag from the opening fence
    pub lang: Option<String>,
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
    pub cost: Option<Dollars>,
    /// model id for assistant messages
    pub model_id: Option<ModelId>,
    /// whether this message is queued (steering) and hasn't been processed yet
    pub queued: bool,
}

impl DisplayMessage {
    #[must_use]
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        }
    }
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
    /// tools with the same batch ran in parallel
    pub batch: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Done,
    Error,
}

/// state for a currently executing tool (shown in the tool panels)
#[derive(Debug, Clone)]
pub struct ActiveToolState {
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub summary: String,
    pub live_output: Option<String>,
    pub status: ToolCallStatus,
    /// output text (set when tool completes)
    pub output: Option<String>,
}

/// detected anomaly in cache behaviour between consecutive API calls
#[derive(Debug, Clone, PartialEq)]
pub enum CacheAnomaly {
    /// context_tokens decreased without a compact/clear
    ContextDecrease { prev: TokenCount, curr: TokenCount },
    /// cache_read dropped significantly while cache_write spiked,
    /// suggesting the cached prefix was evicted
    CacheBust {
        prev_cache_read: TokenCount,
        curr_cache_read: TokenCount,
        curr_cache_write: TokenCount,
    },
}

/// compare consecutive API call usages and detect cache anomalies.
///
/// returns an empty vec when prev is None (first call) or when
/// the usage pattern looks normal
#[must_use]
pub fn detect_cache_anomalies(prev: Option<&Usage>, curr: &Usage) -> Vec<CacheAnomaly> {
    let Some(prev) = prev else {
        return Vec::new();
    };

    let mut anomalies = Vec::new();
    let prev_ctx = prev.total_input_tokens();
    let curr_ctx = curr.total_input_tokens();

    // context should grow (or stay the same) without a compact
    if curr_ctx < prev_ctx {
        anomalies.push(CacheAnomaly::ContextDecrease {
            prev: prev_ctx,
            curr: curr_ctx,
        });
    }

    // cache bust: previous call had significant cache_read, this call
    // has much less cache_read with a cache_write spike.
    // threshold: prev cache_read was >50% of prev context, now dropped by >75%
    let prev_read = prev.cache_read_tokens.get();
    let prev_total = prev_ctx.get().max(1);
    let curr_read = curr.cache_read_tokens.get();
    let prev_was_cached = prev_read * 2 > prev_total; // >50% was cached
    let read_dropped = prev_read > 0 && curr_read * 4 < prev_read; // dropped by >75%
    let write_spiked = curr.cache_write_tokens > prev.cache_write_tokens;

    if prev_was_cached && read_dropped && write_spiked {
        anomalies.push(CacheAnomaly::CacheBust {
            prev_cache_read: prev.cache_read_tokens,
            curr_cache_read: curr.cache_read_tokens,
            curr_cache_write: curr.cache_write_tokens,
        });
    }

    anomalies
}

/// cumulative token and cost tracking for the session
#[derive(Debug, Clone, Default)]
pub struct TokenStats {
    /// total cost so far
    pub total_cost: Dollars,
    /// total tokens used (cumulative across all API calls)
    pub total_tokens: TokenCount,
    /// cumulative uncached input tokens
    pub input_tokens: TokenCount,
    /// cumulative output tokens
    pub output_tokens: TokenCount,
    /// cumulative cache-read tokens
    pub cache_read_tokens: TokenCount,
    /// cumulative cache-write tokens
    pub cache_write_tokens: TokenCount,
    /// last call's input tokens (actual context size)
    pub context_tokens: TokenCount,
    /// model's context window size
    pub context_window: TokenCount,
    /// usage from the previous API call (for anomaly detection)
    prev_usage: Option<Usage>,
}

impl TokenStats {
    /// create with a given context window
    #[must_use]
    pub fn new(context_window: TokenCount) -> Self {
        Self {
            context_window,
            ..Default::default()
        }
    }

    /// accumulate usage from an API call, returning any detected cache anomalies
    pub fn update(&mut self, usage: &Usage, cost: Option<Dollars>) -> Vec<CacheAnomaly> {
        if let Some(c) = cost {
            self.total_cost += c;
        }

        let anomalies = detect_cache_anomalies(self.prev_usage.as_ref(), usage);
        for anomaly in &anomalies {
            match anomaly {
                CacheAnomaly::ContextDecrease { prev, curr } => {
                    tracing::warn!(
                        prev_context = prev.get(),
                        curr_context = curr.get(),
                        delta = prev.get() - curr.get(),
                        "cache anomaly: context decreased without compact"
                    );
                }
                CacheAnomaly::CacheBust {
                    prev_cache_read,
                    curr_cache_read,
                    curr_cache_write,
                } => {
                    tracing::warn!(
                        prev_cache_read = prev_cache_read.get(),
                        curr_cache_read = curr_cache_read.get(),
                        curr_cache_write = curr_cache_write.get(),
                        "cache anomaly: probable cache bust (prefix evicted)"
                    );
                }
            }
        }

        self.total_tokens += usage.total_tokens();
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens;
        self.cache_write_tokens += usage.cache_write_tokens;
        self.context_tokens = usage.total_input_tokens();
        self.prev_usage = Some(*usage);

        anomalies
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
    /// in-progress assistant response
    pub stream: StreamingState,
    /// user input buffer with cursor, images, scroll
    pub input: InputBuffer,
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
    /// tool call being confirmed
    pub confirm_tool_call_id: Option<ToolCallId>,
    /// whether to show prompt injection previews in the chat
    pub show_prompt_injection: bool,
    /// whether to show dollar cost in status bar
    pub show_cost: bool,
    /// selected message index in scroll mode (for copy)
    pub selected_message: Option<usize>,
    /// anchor for visual selection range (v in scroll mode)
    pub selection_anchor: Option<usize>,
    /// what j/k navigates in scroll mode
    pub scroll_unit: ScrollUnit,
    /// selected code block index in block scroll mode
    pub selected_block: Option<usize>,
    /// search state
    pub search: SearchState,
    /// image render positions (populated by MessageList during render)
    pub image_render_areas: RefCell<Vec<ImageRenderArea>>,
    /// per-message wrapped-line ranges (populated by MessageList during render)
    pub message_row_ranges: RefCell<Vec<MessageRowRange>>,
    /// message area rect from last render
    pub message_area: Cell<Rect>,
    /// scroll value from last render (wrapped lines scrolled past)
    pub render_scroll: Cell<u16>,
    /// cached markdown rendering for stable message content
    pub markdown_cache: RefCell<std::collections::HashMap<String, ratatui::text::Text<'static>>>,
    /// cached markdown rendering for the current visible streaming text
    pub stream_markdown_cache: RefCell<Option<(String, ratatui::text::Text<'static>)>>,
    /// working directory (with ~ for home)
    pub cwd: String,
    /// total content lines (set during render by MessageList)
    pub total_content_lines: Cell<u16>,
    /// visible area height (set during render by MessageList)
    pub visible_area_height: Cell<u16>,
    /// pane info: (this pane index 1-based, total panes), None when single pane
    pub pane_info: Option<(u16, u16)>,
    /// background pane alert text (e.g. "pane 2: busy")
    pub background_alert: Option<String>,
    /// prompt cache warmth tracking
    pub cache: CacheTimer,
    /// batch counter for grouping parallel tool calls
    current_tool_batch: u32,
    /// oauth usage data (5h and 7d rolling windows)
    pub oauth_usage: Option<mush_ai::oauth::usage::OAuthUsage>,
    /// colour theme for all widgets
    pub theme: crate::theme::Theme,
}

/// position computed during render for inline image overlay
#[derive(Debug, Clone)]
pub struct ImageRenderArea {
    pub msg_idx: usize,
    pub tc_idx: usize,
    pub area: Rect,
}

/// wrapped-line range for a message (populated during render)
#[derive(Debug, Clone)]
pub struct MessageRowRange {
    pub msg_idx: usize,
    /// first wrapped line (absolute, includes bottom-anchor padding)
    pub start: u16,
    /// first wrapped line after this message
    pub end: u16,
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
    pub fn new(model_id: ModelId, context_window: TokenCount) -> Self {
        Self {
            messages: Vec::new(),
            stream: StreamingState::new(),
            input: InputBuffer::new(),
            scroll_offset: 0,
            model_id,
            stats: TokenStats::new(context_window),
            should_quit: false,
            status: None,
            active_tools: Vec::new(),
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
            confirm_tool_call_id: None,
            show_prompt_injection: false,
            show_cost: false,
            selected_message: None,
            selection_anchor: None,
            scroll_unit: ScrollUnit::default(),
            selected_block: None,
            search: SearchState::default(),
            image_render_areas: RefCell::new(Vec::new()),
            message_row_ranges: RefCell::new(Vec::new()),
            message_area: Cell::new(Rect::default()),
            render_scroll: Cell::new(0),
            markdown_cache: RefCell::new(std::collections::HashMap::new()),
            stream_markdown_cache: RefCell::new(None),
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
            pane_info: None,
            background_alert: None,
            cache: CacheTimer::new(300),
            current_tool_batch: 0,
            oauth_usage: None,
            theme: crate::theme::Theme::default(),
        }
    }

    /// advance the spinner state (throttled to ~8fps from ~60fps frame rate)
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        if self.tick_count.is_multiple_of(TICK_DIVISOR) {
            self.throbber_state.calc_next();
        }
        if self.stream.active {
            self.stream.advance_typewriter();
        }
    }

    /// whether the unread flash indicator is in the "on" phase
    /// cycles at ~1hz (30 ticks on, 30 off at ~60fps)
    pub fn unread_flash_on(&self) -> bool {
        self.tick_count % UNREAD_FLASH_CYCLE < UNREAD_FLASH_ON
    }

    /// whether the agent is currently active (streaming or executing tools)
    pub fn is_busy(&self) -> bool {
        self.stream.active
            || self
                .active_tools
                .iter()
                .any(|t| t.status == ToolCallStatus::Running)
    }

    /// add a user message to the display
    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages
            .push(DisplayMessage::new(MessageRole::User, text));
        self.scroll_offset = 0;
    }

    /// remove all queued steering messages, returning their text content
    pub fn take_queued_messages(&mut self) -> Vec<String> {
        let mut texts = Vec::new();
        self.messages.retain(|m| {
            if m.queued {
                texts.push(m.content.clone());
                false
            } else {
                true
            }
        });
        texts
    }

    /// add a queued steering message (shown dimmed until processed)
    pub fn push_queued_message(&mut self, text: impl Into<String>) {
        self.messages.push(DisplayMessage {
            queued: true,
            ..DisplayMessage::new(MessageRole::User, text)
        });
        self.scroll_offset = 0;
    }

    /// remove the last queued steering message from display, return its text
    pub fn pop_last_queued_message(&mut self) -> Option<String> {
        let idx = self.messages.iter().rposition(|m| m.queued)?;
        let msg = self.messages.remove(idx);
        Some(msg.content)
    }

    /// start streaming a new assistant message
    pub fn start_streaming(&mut self) {
        self.stream.start();
        self.scroll_offset = 0;
    }

    /// append text delta to the current stream
    pub fn push_text_delta(&mut self, delta: &str) {
        self.stream.text.push_str(delta);
    }

    /// append thinking delta to the current stream
    pub fn push_thinking_delta(&mut self, delta: &str) {
        self.stream.thinking.push_str(delta);
    }

    /// visible portion of streaming text (typewriter effect)
    pub fn visible_streaming_text(&self) -> &str {
        self.stream.visible_text()
    }

    /// visible portion of streaming thinking (typewriter effect)
    pub fn visible_streaming_thinking(&self) -> &str {
        self.stream.visible_thinking()
    }

    /// accumulate streaming tool call arguments
    pub fn push_tool_args_delta(&mut self, delta: &str) {
        self.stream.tool_args.push_str(delta);
    }

    /// mark a tool as being executed
    pub fn start_tool(&mut self, tool_call_id: &ToolCallId, name: &str, summary: &str) {
        // new batch when no tools are currently running
        let has_running = self
            .active_tools
            .iter()
            .any(|t| t.status == ToolCallStatus::Running);
        if !has_running {
            self.current_tool_batch += 1;
        }
        self.active_tools.push(ActiveToolState {
            tool_call_id: tool_call_id.clone(),
            name: name.to_string(),
            summary: summary.to_string(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });
        self.stream.tool_args.clear();
        // add to the last message's tool calls if we have one in progress
        if let Some(last) = self.messages.last_mut() {
            last.tool_calls.push(DisplayToolCall {
                name: name.to_string(),
                summary: summary.to_string(),
                status: ToolCallStatus::Running,
                output_preview: None,
                image_data: None,
                batch: self.current_tool_batch,
            });
        }
    }

    /// expand a batch tool into individual sub-tool display entries
    pub fn start_batch_tool(
        &mut self,
        tool_call_id: &ToolCallId,
        summary: &str,
        sub_calls: &[(String, String)], // (tool_name, summary) per sub-call
    ) {
        self.current_tool_batch += 1;
        // one active tool state for the live panel
        self.active_tools.push(ActiveToolState {
            tool_call_id: tool_call_id.clone(),
            name: "batch".to_string(),
            summary: summary.to_string(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });
        self.stream.tool_args.clear();
        // individual display entries for message history
        if let Some(last) = self.messages.last_mut() {
            for (name, sub_summary) in sub_calls {
                last.tool_calls.push(DisplayToolCall {
                    name: name.clone(),
                    summary: sub_summary.clone(),
                    status: ToolCallStatus::Running,
                    output_preview: None,
                    image_data: None,
                    batch: self.current_tool_batch,
                });
            }
        }
    }

    /// finish a batch tool, distributing results to individual sub-calls
    pub fn end_batch_tool(&mut self, tool_call_id: &ToolCallId, output: Option<&str>) {
        // mark the active tool as done
        if let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| &t.tool_call_id == tool_call_id)
        {
            tool.status = ToolCallStatus::Done;
            tool.output = output.map(truncate_output);
            tool.live_output = None;
        }
        // parse output into per-sub-call sections and update display entries
        let Some(text) = output else { return };
        let sections = parse_batch_output(text);
        if let Some(last) = self.messages.last_mut() {
            // find the running batch sub-calls (they're the last N running entries)
            let running: Vec<usize> = last
                .tool_calls
                .iter()
                .enumerate()
                .filter(|(_, tc)| tc.status == ToolCallStatus::Running)
                .map(|(i, _)| i)
                .collect();

            for (section_idx, section) in sections.iter().enumerate() {
                if let Some(&tc_idx) = running.get(section_idx) {
                    let tc = &mut last.tool_calls[tc_idx];
                    tc.status = if section.is_error {
                        ToolCallStatus::Error
                    } else {
                        ToolCallStatus::Done
                    };
                    if !section.content.is_empty() {
                        tc.output_preview = Some(truncate_output(&section.content));
                    }
                }
            }
            // mark any remaining unmatched sub-calls as done
            for &idx in running.iter().skip(sections.len()) {
                last.tool_calls[idx].status = ToolCallStatus::Done;
            }
        }
    }

    /// mark a tool as done, with optional output preview
    pub fn end_tool(
        &mut self,
        tool_call_id: &ToolCallId,
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
            .find(|t| &t.tool_call_id == tool_call_id)
        {
            tool.status = status;
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
    pub fn push_tool_output(&mut self, tool_call_id: &ToolCallId, output: &str) {
        if let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| &t.tool_call_id == tool_call_id)
        {
            tool.live_output = Some(output.to_string());
        }
    }

    /// finish streaming, create the assistant message
    pub fn finish_streaming(&mut self, usage: Option<Usage>, cost: Option<Dollars>) {
        let (text, thinking) = self.stream.take();

        let assistant_msg = DisplayMessage {
            thinking,
            thinking_expanded: self.thinking_display == ThinkingDisplay::Expanded,
            usage,
            cost,
            model_id: Some(self.model_id.clone()),
            ..DisplayMessage::new(MessageRole::Assistant, text.trim_start_matches('\n'))
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
            let anomalies = self.stats.update(u, cost);
            for anomaly in &anomalies {
                let msg = match anomaly {
                    CacheAnomaly::ContextDecrease { prev, curr } => {
                        format!(
                            "⚠ context decreased: {}k → {}k (delta -{}k) without compact",
                            prev.get() / 1000,
                            curr.get() / 1000,
                            (prev.get() - curr.get()) / 1000,
                        )
                    }
                    CacheAnomaly::CacheBust {
                        prev_cache_read,
                        curr_cache_read,
                        curr_cache_write,
                    } => {
                        format!(
                            "⚠ probable cache bust: cache_read {}k → {}k, cache_write {}k (prefix evicted)",
                            prev_cache_read.get() / 1000,
                            curr_cache_read.get() / 1000,
                            curr_cache_write.get() / 1000,
                        )
                    }
                };
                self.push_system_message(msg);
            }
        } else if let Some(c) = cost {
            self.stats.total_cost += c;
        }
        if self.scroll_offset > 0 {
            self.has_unread = true;
        }
    }

    /// insert a character at the cursor (clears tab completion)
    pub fn input_char(&mut self, c: char) {
        self.tab_state = None;
        self.input.insert_char(c);
    }

    /// cycle through tab completions for the current input
    pub fn tab_complete(&mut self) {
        if let Some(ref mut state) = self.tab_state {
            state.index = (state.index + 1) % state.matches.len();
            let replacement = &state.matches[state.index];
            self.input.text = replacement.clone();
            self.input.cursor = self.input.text.len();
            self.input.ensure_cursor_visible();
            return;
        }

        let text = self.input.text.as_str();
        let matches: Vec<String> = if let Some(rest) = text.strip_prefix("/model ") {
            self.completions
                .iter()
                .filter(|c| !c.starts_with('/'))
                .filter(|c| c.starts_with(rest))
                .map(|c| format!("/model {c}"))
                .collect()
        } else if text.starts_with('/') {
            self.completions
                .iter()
                .filter(|c| c.starts_with(text))
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
        self.input.text = first;
        self.input.cursor = self.input.text.len();
        self.input.ensure_cursor_visible();
    }

    /// open the slash command completion menu, filtering by current input
    pub fn open_slash_menu(&mut self) {
        let prefix = self.input.text.as_str();

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
            let prefix = self.input.text.as_str();

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

    /// whether visual selection mode is active (v in scroll mode)
    pub fn has_selection(&self) -> bool {
        self.selection_anchor.is_some() && self.selected_message.is_some()
    }

    /// get the inclusive selection range (min..=max), if active
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let cursor = self.selected_message?;
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    /// find which message is displayed at a given screen row
    ///
    /// uses render metadata (message_row_ranges, render_scroll, message_area)
    /// populated by MessageList during the last render pass
    pub fn message_at_screen_row(&self, row: u16) -> Option<usize> {
        let area = self.message_area.get();
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let screen_line = self.render_scroll.get() + (row - area.y);
        let ranges = self.message_row_ranges.borrow();
        ranges
            .iter()
            .find(|r| screen_line >= r.start && screen_line < r.end)
            .map(|r| r.msg_idx)
    }

    /// extract all fenced code blocks from conversation messages
    pub fn code_blocks(&self) -> Vec<CodeBlock> {
        let mut blocks = Vec::new();
        for (msg_idx, msg) in self.messages.iter().enumerate() {
            let mut in_block = false;
            let mut lang = None;
            let mut lines: Vec<&str> = Vec::new();

            for raw_line in msg.content.lines() {
                if raw_line.starts_with("```") {
                    if in_block {
                        blocks.push(CodeBlock {
                            msg_idx,
                            content: lines.join("\n"),
                            lang: lang.take(),
                        });
                        lines.clear();
                        in_block = false;
                    } else {
                        let tag = raw_line.trim_start_matches('`').trim();
                        lang = if tag.is_empty() {
                            None
                        } else {
                            Some(tag.to_string())
                        };
                        in_block = true;
                    }
                    continue;
                }
                if in_block {
                    lines.push(raw_line);
                }
            }

            // unclosed fence
            if in_block && !lines.is_empty() {
                blocks.push(CodeBlock {
                    msg_idx,
                    content: lines.join("\n"),
                    lang: lang.take(),
                });
            }
        }
        blocks
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
        if self.tab_state.is_some() {
            return None;
        }
        if self.input.cursor != self.input.text.len() || self.input.text.is_empty() {
            return None;
        }
        let text = self.input.text.as_str();
        let candidate = if let Some(rest) = text.strip_prefix("/model ") {
            self.completions
                .iter()
                .filter(|c| !c.starts_with('/'))
                .find(|c| c.starts_with(rest))
                .map(|c| &c[rest.len()..])
        } else if text.starts_with('/') {
            self.completions
                .iter()
                .find(|c| c.starts_with(text) && c.len() > text.len())
                .map(|c| &c[text.len()..])
        } else {
            None
        };
        candidate.filter(|s| !s.is_empty())
    }

    /// clear all messages (for /clear command)
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.stream = StreamingState::new();
        self.scroll_offset = 0;
        self.stats.reset();
        self.input.images.clear();
    }

    /// push a system message to the display
    pub fn push_system_message(&mut self, text: impl Into<String>) {
        self.messages
            .push(DisplayMessage::new(MessageRole::System, text));
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
            ThinkingLevel::Off => ThinkingLevel::Low,
            ThinkingLevel::Minimal => ThinkingLevel::Low,
            ThinkingLevel::Low => ThinkingLevel::Medium,
            ThinkingLevel::Medium => ThinkingLevel::High,
            ThinkingLevel::High => ThinkingLevel::Xhigh,
            ThinkingLevel::Xhigh => ThinkingLevel::Off,
        };
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

/// parsed section from batch tool output
pub(crate) struct BatchSection {
    pub is_error: bool,
    pub content: String,
}

/// parse batch output into per-sub-call sections
///
/// format: `--- [N] ToolName [ok|error] ---\ncontent\n\n`
pub(crate) fn parse_batch_output(text: &str) -> Vec<BatchSection> {
    let mut sections = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        // match header: "--- [N] ToolName [ok|error] ---"
        if line.starts_with("--- [") && line.ends_with("] ---") {
            let is_error = line.contains("[error]");
            i += 1;
            // collect content until next header or summary line
            let mut content = String::new();
            while i < lines.len() {
                let next = lines[i];
                if (next.starts_with("--- [") && next.ends_with("] ---"))
                    || next.starts_with("batch: ")
                {
                    break;
                }
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(next);
                i += 1;
            }
            sections.push(BatchSection {
                is_error,
                content: content.trim().to_string(),
            });
        } else {
            i += 1;
        }
    }

    sections
}

pub(crate) fn truncate_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    let preview: Vec<String> = lines
        .into_iter()
        .take(MAX_PREVIEW_LINES)
        .map(|l| {
            if l.len() > MAX_PREVIEW_LINE_LEN {
                let end = l.floor_char_boundary(MAX_PREVIEW_LINE_LEN);
                format!("{}...", &l[..end])
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
        let app = App::new("test-model".into(), TokenCount::new(200_000));
        assert!(app.messages.is_empty());
        assert!(!app.stream.active);
        assert!(app.input.text.is_empty());
        assert_eq!(app.input.cursor, 0);
    }

    #[test]
    fn push_user_message() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_user_message("hello");
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::User);
        assert_eq!(app.messages[0].content, "hello");
    }

    #[test]
    fn streaming_lifecycle() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        assert!(app.stream.active);

        app.push_text_delta("hello ");
        app.push_text_delta("world");
        assert_eq!(app.stream.text, "hello world");

        app.finish_streaming(None, None);
        assert!(!app.stream.active);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "hello world");
        assert!(app.stream.text.is_empty());
    }

    #[test]
    fn streaming_with_thinking() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_user_message("do something");
        // simulate assistant message already pushed by finish_streaming
        app.messages
            .push(DisplayMessage::new(MessageRole::Assistant, ""));

        let tool_call_id = ToolCallId::from("tc_1");
        app.start_tool(&tool_call_id, "bash", "ls -la");
        assert_eq!(app.active_tools.len(), 1);
        assert_eq!(app.active_tools[0].name, "bash");
        assert_eq!(app.messages.last().unwrap().tool_calls.len(), 1);
        assert_eq!(
            app.messages.last().unwrap().tool_calls[0].status,
            ToolCallStatus::Running
        );

        app.end_tool(
            &tool_call_id,
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.area.set(Rect::new(0, 0, 20, 8));
        app.input.visible_lines.set(2);
        app.input.total_lines.set(2); // stale from previous render
        app.input.text = "one\ntwo\nthree\nfour\nfive".into();
        app.input.cursor = app.input.text.len();

        app.input.ensure_cursor_visible();

        assert!(app.input.total_lines.get() >= 5);
        assert!(app.input.scroll.get() > 0);
    }

    #[test]
    fn input_editing() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input_char('h');
        app.input_char('i');
        assert_eq!(app.input.text, "hi");
        assert_eq!(app.input.cursor, 2);

        app.input.cursor_left();
        assert_eq!(app.input.cursor, 1);
        app.input_char('!');
        assert_eq!(app.input.text, "h!i");

        app.input.cursor_home();
        assert_eq!(app.input.cursor, 0);
        app.input.cursor_end();
        assert_eq!(app.input.cursor, 3);

        app.input.backspace();
        assert_eq!(app.input.text, "h!");
    }

    #[test]
    fn input_delete() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "abc".into();
        app.input.cursor = 1;
        app.input.delete();
        assert_eq!(app.input.text, "ac");
    }

    #[test]
    fn take_input_resets() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = 3;
        let text = app.input.take_text();
        assert_eq!(text, "hello");
        assert!(app.input.text.is_empty());
        assert_eq!(app.input.cursor, 0);
    }

    #[test]
    fn cost_accumulates() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("a");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(100),
                output_tokens: TokenCount::new(50),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::ZERO,
            }),
            Some(Dollars::new(0.005)),
        );

        app.start_streaming();
        app.push_text_delta("b");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(200),
                output_tokens: TokenCount::new(100),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::ZERO,
            }),
            Some(Dollars::new(0.01)),
        );

        assert!((app.stats.total_cost.get() - 0.015).abs() < f64::EPSILON);
        assert_eq!(app.stats.total_tokens, TokenCount::new(450));
        assert_eq!(app.stats.input_tokens, TokenCount::new(300));
        assert_eq!(app.stats.output_tokens, TokenCount::new(150));
        assert_eq!(app.stats.cache_read_tokens, TokenCount::ZERO);
        assert_eq!(app.stats.cache_write_tokens, TokenCount::ZERO);
        assert_eq!(app.stats.context_tokens, TokenCount::new(200));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages
            .push(DisplayMessage::new(MessageRole::Assistant, ""));
        let tool_call_id = ToolCallId::from("tc_1");
        app.start_tool(&tool_call_id, "read", "src/main.rs");
        app.end_tool(
            &tool_call_id,
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
    fn batch_tool_expands_into_sub_calls() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages
            .push(DisplayMessage::new(MessageRole::Assistant, ""));

        let tc_id = ToolCallId::from("tc_batch");
        app.start_batch_tool(
            &tc_id,
            "3 tool calls",
            &[
                ("read".into(), "a.rs".into()),
                ("read".into(), "b.rs".into()),
                ("bash".into(), "cargo test".into()),
            ],
        );

        // one active tool for the live panel
        assert_eq!(app.active_tools.len(), 1);
        assert_eq!(app.active_tools[0].name, "batch");
        // three display entries in the message
        let tcs = &app.messages.last().unwrap().tool_calls;
        assert_eq!(tcs.len(), 3);
        assert_eq!(tcs[0].name, "read");
        assert_eq!(tcs[1].name, "read");
        assert_eq!(tcs[2].name, "bash");
        // all same batch
        assert_eq!(tcs[0].batch, tcs[1].batch);
        assert_eq!(tcs[1].batch, tcs[2].batch);
        assert!(tcs.iter().all(|t| t.status == ToolCallStatus::Running));
    }

    #[test]
    fn batch_end_distributes_results() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages
            .push(DisplayMessage::new(MessageRole::Assistant, ""));

        let tc_id = ToolCallId::from("tc_batch");
        app.start_batch_tool(
            &tc_id,
            "2 tool calls",
            &[
                ("read".into(), "a.rs".into()),
                ("bash".into(), "cargo test".into()),
            ],
        );

        let output = "\
--- [0] read [ok] ---
fn main() {}

--- [1] bash [error] ---
error: could not compile

batch: 1/2 succeeded, 1 failed";

        app.end_batch_tool(&tc_id, Some(output));

        let tcs = &app.messages.last().unwrap().tool_calls;
        assert_eq!(tcs[0].status, ToolCallStatus::Done);
        assert!(tcs[0].output_preview.as_ref().unwrap().contains("fn main"));
        assert_eq!(tcs[1].status, ToolCallStatus::Error);
        assert!(
            tcs[1]
                .output_preview
                .as_ref()
                .unwrap()
                .contains("could not compile")
        );
    }

    #[test]
    fn parse_batch_output_splits_sections() {
        let text = "\
--- [0] read [ok] ---
content a

--- [1] bash [error] ---
error text

batch: 1/2 succeeded, 1 failed";

        let sections = parse_batch_output(text);
        assert_eq!(sections.len(), 2);
        assert!(!sections[0].is_error);
        assert_eq!(sections[0].content, "content a");
        assert!(sections[1].is_error);
        assert_eq!(sections[1].content, "error text");
    }

    #[test]
    fn toggle_thinking() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));

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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.thinking_display = ThinkingDisplay::Collapse;
        app.start_streaming();
        app.push_thinking_delta("deep thoughts");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        assert!(!app.messages[0].thinking_expanded);
    }

    #[test]
    fn thinking_display_hidden_starts_collapsed() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.thinking_display = ThinkingDisplay::Hidden;
        app.start_streaming();
        app.push_thinking_delta("deep thoughts");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        assert!(!app.messages[0].thinking_expanded);
    }

    #[test]
    fn multi_line_input() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input_char('a');
        app.input_char('\n');
        app.input_char('b');
        assert_eq!(app.input.text, "a\nb");
        assert_eq!(app.input.cursor, 3);
    }

    #[test]
    fn session_picker_open_close() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
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

        let mut app = App::new("test".into(), TokenCount::new(200_000));
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

        let mut app = App::new("test".into(), TokenCount::new(200_000));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_tool_args_delta("{\"path\":");
        app.push_tool_args_delta("\"src/");
        assert_eq!(app.stream.tool_args, "{\"path\":\"src/");

        // start_tool clears the buffer
        let tool_call_id = ToolCallId::from("tc_1");
        app.start_tool(&tool_call_id, "read", "src/main.rs");
        assert!(app.stream.tool_args.is_empty());
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
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

    #[test]
    fn cache_ttl_secs_by_provider() {
        use super::cache_ttl_secs;

        // anthropic: short = 5 min, long = 1 hour, none = 0
        assert_eq!(
            cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Short)),
            300
        );
        assert_eq!(
            cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Long)),
            3600
        );
        assert_eq!(
            cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::None)),
            0
        );
        assert_eq!(cache_ttl_secs(&Provider::Anthropic, None), 300); // default = short

        // openrouter / custom: defaults to 300
        assert_eq!(cache_ttl_secs(&Provider::OpenRouter, None), 300);
        assert_eq!(cache_ttl_secs(&Provider::Custom("xai".into()), None), 300);
    }

    #[test]
    fn cache_remaining_countdown() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        assert!(app.cache.remaining_secs().is_none());

        app.cache.ttl_secs = 300;
        app.cache.refresh();
        let remaining = app.cache.remaining_secs().unwrap();
        // just refreshed, should be very close to 300
        assert!((298..=300).contains(&remaining));
    }

    #[test]
    fn pop_last_queued_message() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        // no queued messages returns none
        assert!(app.pop_last_queued_message().is_none());

        // add some queued messages
        app.push_queued_message("first steering");
        app.push_queued_message("second steering");

        // pops the last one
        assert_eq!(app.pop_last_queued_message().unwrap(), "second steering");
        assert_eq!(app.pop_last_queued_message().unwrap(), "first steering");
        assert!(app.pop_last_queued_message().is_none());
    }

    #[test]
    fn pop_queued_skips_non_queued() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_user_message("normal message");
        app.push_queued_message("steering msg");

        // only pops the queued one
        assert_eq!(app.pop_last_queued_message().unwrap(), "steering msg");
        assert!(app.pop_last_queued_message().is_none());
        // normal message still there
        assert_eq!(app.messages.len(), 1);
        assert!(!app.messages[0].queued);
    }

    #[test]
    fn selection_range_none_without_anchor() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        assert!(!app.has_selection());
        assert!(app.selection_range().is_none());
    }

    #[test]
    fn selection_range_normalises_order() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        // anchor at 3, cursor at 1 => range (1, 3)
        app.selection_anchor = Some(3);
        app.selected_message = Some(1);
        assert!(app.has_selection());
        assert_eq!(app.selection_range(), Some((1, 3)));

        // anchor at 1, cursor at 3 => range (1, 3)
        app.selection_anchor = Some(1);
        app.selected_message = Some(3);
        assert_eq!(app.selection_range(), Some((1, 3)));

        // same index => single message range
        app.selection_anchor = Some(2);
        app.selected_message = Some(2);
        assert_eq!(app.selection_range(), Some((2, 2)));
    }

    #[test]
    fn tool_call_status_is_copy() {
        let status = ToolCallStatus::Running;
        let copied = status; // would fail if not Copy
        assert_eq!(status, copied);
    }

    #[test]
    fn display_message_new_has_sane_defaults() {
        let msg = DisplayMessage::new(MessageRole::User, "hello");
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "hello");
        assert!(msg.tool_calls.is_empty());
        assert!(msg.thinking.is_none());
        assert!(!msg.thinking_expanded);
        assert!(msg.usage.is_none());
        assert!(msg.cost.is_none());
        assert!(msg.model_id.is_none());
        assert!(!msg.queued);
    }

    #[test]
    fn display_message_new_with_struct_update() {
        let msg = DisplayMessage {
            queued: true,
            ..DisplayMessage::new(MessageRole::User, "steer")
        };
        assert!(msg.queued);
        assert_eq!(msg.content, "steer");
    }

    #[test]
    fn cache_timer_inactive_by_default() {
        let timer = CacheTimer::new(300);
        assert!(timer.remaining_secs().is_none());
        assert!(timer.elapsed_secs().is_none());
        assert!(!timer.warn_sent);
        assert!(!timer.expired_sent);
    }

    #[test]
    fn cache_timer_refresh_starts_countdown() {
        let mut timer = CacheTimer::new(300);
        timer.refresh();
        let remaining = timer.remaining_secs().unwrap();
        assert!((298..=300).contains(&remaining));
        assert!(timer.elapsed_secs().unwrap() <= 2);
    }

    #[test]
    fn cache_timer_refresh_resets_flags() {
        let mut timer = CacheTimer::new(300);
        timer.warn_sent = true;
        timer.expired_sent = true;
        timer.refresh();
        assert!(!timer.warn_sent);
        assert!(!timer.expired_sent);
    }

    #[test]
    fn cache_timer_disabled_when_zero_ttl() {
        let timer = CacheTimer::new(0);
        assert_eq!(timer.ttl_secs, 0);
    }

    #[test]
    fn streaming_state_starts_inactive() {
        let stream = StreamingState::new();
        assert!(!stream.active);
        assert!(stream.text.is_empty());
        assert!(stream.thinking.is_empty());
        assert!(stream.tool_args.is_empty());
    }

    #[test]
    fn streaming_state_start_clears_and_activates() {
        let mut stream = StreamingState::new();
        stream.text.push_str("leftover");
        stream.start();
        assert!(stream.active);
        assert!(stream.text.is_empty());
        assert!(stream.thinking.is_empty());
    }

    #[test]
    fn streaming_state_visible_text_typewriter() {
        let mut stream = StreamingState::new();
        stream.start();
        stream.text.push_str("hello world");
        // before advance, nothing visible
        assert_eq!(stream.visible_text(), "");
        // advance once
        stream.advance_typewriter();
        let visible = stream.visible_text();
        assert!(!visible.is_empty());
        assert!(visible.len() <= "hello world".len());
        // enough advances catch up
        for _ in 0..20 {
            stream.advance_typewriter();
        }
        assert_eq!(stream.visible_text(), "hello world");
    }

    #[test]
    fn streaming_state_take_returns_content() {
        let mut stream = StreamingState::new();
        stream.start();
        stream.text.push_str("answer");
        stream.thinking.push_str("reasoning");
        let (text, thinking) = stream.take();
        assert_eq!(text, "answer");
        assert_eq!(thinking.as_deref(), Some("reasoning"));
        assert!(!stream.active);
        assert!(stream.text.is_empty());
    }

    #[test]
    fn streaming_state_take_no_thinking_returns_none() {
        let mut stream = StreamingState::new();
        stream.start();
        stream.text.push_str("just text");
        let (_, thinking) = stream.take();
        assert!(thinking.is_none());
    }

    #[test]
    fn tick_throttles_spinner() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let initial = app.throbber_state.clone();
        // single tick should not advance (throttled)
        app.tick();
        // but TICK_DIVISOR ticks should advance
        for _ in 1..TICK_DIVISOR {
            app.tick();
        }
        assert_ne!(format!("{:?}", app.throbber_state), format!("{initial:?}"));
    }

    #[test]
    fn unread_flash_cycle_works() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.has_unread = true;
        // at tick 0, flash should be on
        assert!(app.unread_flash_on());
        // advance past the on-phase
        for _ in 0..UNREAD_FLASH_ON {
            app.tick();
        }
        assert!(!app.unread_flash_on());
    }

    #[test]
    fn input_buffer_basic_editing() {
        let mut buf = InputBuffer::new();
        buf.insert_char('h');
        buf.insert_char('i');
        assert_eq!(buf.text, "hi");
        assert_eq!(buf.cursor, 2);

        buf.backspace();
        assert_eq!(buf.text, "h");
        assert_eq!(buf.cursor, 1);

        buf.cursor_home();
        assert_eq!(buf.cursor, 0);

        buf.cursor_end();
        assert_eq!(buf.cursor, 1);
    }

    #[test]
    fn input_buffer_take_returns_and_clears() {
        let mut buf = InputBuffer::new();
        buf.insert_char('x');
        let taken = buf.take_text();
        assert_eq!(taken, "x");
        assert!(buf.text.is_empty());
        assert_eq!(buf.cursor, 0);
    }

    #[test]
    fn input_buffer_word_navigation() {
        let mut buf = InputBuffer::new();
        buf.text = "hello world foo".into();
        buf.cursor = buf.text.len();

        buf.cursor_word_left();
        assert_eq!(buf.cursor, 12); // before "foo"

        buf.cursor_word_left();
        assert_eq!(buf.cursor, 6); // before "world"
    }

    #[test]
    fn code_blocks_extracts_fenced_blocks() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "here:\n```bash\nrm -rf mything\n```\nand also:\n```python\nprint(\"hello\")\n```",
        ));
        app.messages
            .push(DisplayMessage::new(MessageRole::User, "nice"));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "one more:\n```\nplain block\n```",
        ));

        let blocks = app.code_blocks();
        assert_eq!(blocks.len(), 3);

        assert_eq!(blocks[0].msg_idx, 0);
        assert_eq!(blocks[0].content, "rm -rf mything");
        assert_eq!(blocks[0].lang.as_deref(), Some("bash"));

        assert_eq!(blocks[1].msg_idx, 0);
        assert_eq!(blocks[1].content, "print(\"hello\")");
        assert_eq!(blocks[1].lang.as_deref(), Some("python"));

        assert_eq!(blocks[2].msg_idx, 2);
        assert_eq!(blocks[2].content, "plain block");
        assert_eq!(blocks[2].lang, None);
    }

    #[test]
    fn code_blocks_handles_unclosed_fence() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            "streaming:\n```rust\nfn main() {}",
        ));

        let blocks = app.code_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "fn main() {}");
        assert_eq!(blocks[0].lang.as_deref(), Some("rust"));
    }

    #[test]
    fn insert_str_at_end() {
        let mut buf = InputBuffer::new();
        buf.insert_str("hello world");
        assert_eq!(buf.text, "hello world");
        assert_eq!(buf.cursor, 11);
    }

    #[test]
    fn insert_str_at_middle() {
        let mut buf = InputBuffer::new();
        buf.text = "hello world".into();
        buf.cursor = 5;
        buf.insert_str(" beautiful");
        assert_eq!(buf.text, "hello beautiful world");
        assert_eq!(buf.cursor, 15);
    }

    #[test]
    fn insert_str_multiline_paste() {
        let mut buf = InputBuffer::new();
        buf.insert_str("line 1\nline 2\nline 3");
        assert_eq!(buf.text, "line 1\nline 2\nline 3");
        assert_eq!(buf.cursor, 20);
    }

    #[test]
    fn insert_str_empty_is_noop() {
        let mut buf = InputBuffer::new();
        buf.text = "existing".into();
        buf.cursor = 3;
        buf.insert_str("");
        assert_eq!(buf.text, "existing");
        assert_eq!(buf.cursor, 3);
    }

    // -- cache anomaly detection tests --

    #[test]
    fn detect_context_decrease() {
        let prev = Usage {
            input_tokens: TokenCount::new(10_000),
            output_tokens: TokenCount::new(5_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::ZERO,
        };
        let curr = Usage {
            input_tokens: TokenCount::new(8_000),
            output_tokens: TokenCount::new(6_000),
            cache_read_tokens: TokenCount::new(70_000),
            cache_write_tokens: TokenCount::ZERO,
        };
        let anomalies = detect_cache_anomalies(Some(&prev), &curr);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, CacheAnomaly::ContextDecrease { .. })),
            "should detect context decrease: prev=100k, curr=78k"
        );
    }

    #[test]
    fn detect_cache_bust() {
        let prev = Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::new(95_000),
            cache_write_tokens: TokenCount::ZERO,
        };
        let curr = Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::new(100_000),
        };
        let anomalies = detect_cache_anomalies(Some(&prev), &curr);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, CacheAnomaly::CacheBust { .. })),
            "should detect cache bust: cache_read dropped from 95k to 0"
        );
    }

    #[test]
    fn no_anomaly_on_normal_growth() {
        let prev = Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::new(90_000),
            cache_write_tokens: TokenCount::new(5_000),
        };
        let curr = Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(4_000),
            cache_read_tokens: TokenCount::new(95_000),
            cache_write_tokens: TokenCount::new(5_000),
        };
        let anomalies = detect_cache_anomalies(Some(&prev), &curr);
        assert!(
            anomalies.is_empty(),
            "normal growth should produce no anomalies"
        );
    }

    #[test]
    fn no_anomaly_on_first_call() {
        let curr = Usage {
            input_tokens: TokenCount::new(5_000),
            output_tokens: TokenCount::new(3_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::new(95_000),
        };
        let anomalies = detect_cache_anomalies(None, &curr);
        assert!(
            anomalies.is_empty(),
            "first call should not trigger anomalies"
        );
    }

    #[test]
    fn no_anomaly_after_reset() {
        let mut stats = TokenStats::new(TokenCount::new(200_000));
        // simulate a call
        stats.update(
            &Usage {
                input_tokens: TokenCount::new(10_000),
                output_tokens: TokenCount::new(5_000),
                cache_read_tokens: TokenCount::new(90_000),
                cache_write_tokens: TokenCount::ZERO,
            },
            None,
        );

        stats.reset();

        // next call after reset should not detect anomalies
        let anomalies = stats.update(
            &Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::new(95_000),
            },
            None,
        );
        assert!(
            anomalies.is_empty(),
            "post-reset call should not trigger anomalies"
        );
    }

    #[test]
    fn stats_update_returns_anomalies() {
        let mut stats = TokenStats::new(TokenCount::new(200_000));
        // first call: warm cache
        let a1 = stats.update(
            &Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::new(90_000),
                cache_write_tokens: TokenCount::new(5_000),
            },
            None,
        );
        assert!(a1.is_empty(), "first call should have no anomalies");

        // second call: cache bust (cache_read drops to 0, cache_write spikes)
        let a2 = stats.update(
            &Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::new(100_000),
            },
            None,
        );
        assert!(
            a2.iter()
                .any(|a| matches!(a, CacheAnomaly::CacheBust { .. })),
            "should detect cache bust via stats.update"
        );
    }

    #[test]
    fn finish_streaming_shows_system_message_on_cache_bust() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        // first call: warm cache
        app.start_streaming();
        app.push_text_delta("hello");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::new(90_000),
                cache_write_tokens: TokenCount::new(5_000),
            }),
            None,
        );

        let before = app.messages.len();

        // second call: cache bust
        app.start_streaming();
        app.push_text_delta("world");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::new(100_000),
            }),
            None,
        );

        // should have an extra system message about the cache bust
        let system_msgs: Vec<_> = app.messages[before..]
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .collect();
        assert!(
            system_msgs.iter().any(|m| m.content.contains("cache bust")),
            "expected system message about cache bust, got: {system_msgs:?}"
        );
    }

    #[test]
    fn finish_streaming_shows_system_message_on_context_decrease() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        // first call
        app.start_streaming();
        app.push_text_delta("hello");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(10_000),
                output_tokens: TokenCount::new(5_000),
                cache_read_tokens: TokenCount::new(90_000),
                cache_write_tokens: TokenCount::ZERO,
            }),
            None,
        );

        let before = app.messages.len();

        // second call: context decreased
        app.start_streaming();
        app.push_text_delta("world");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(8_000),
                output_tokens: TokenCount::new(6_000),
                cache_read_tokens: TokenCount::new(70_000),
                cache_write_tokens: TokenCount::ZERO,
            }),
            None,
        );

        let system_msgs: Vec<_> = app.messages[before..]
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .collect();
        assert!(
            system_msgs
                .iter()
                .any(|m| m.content.contains("context decreased")),
            "expected system message about context decrease, got: {system_msgs:?}"
        );
    }

    #[test]
    fn finish_streaming_no_system_message_on_normal_growth() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        // first call
        app.start_streaming();
        app.push_text_delta("hello");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::new(90_000),
                cache_write_tokens: TokenCount::new(5_000),
            }),
            None,
        );

        let before = app.messages.len();

        // second call: normal growth
        app.start_streaming();
        app.push_text_delta("world");
        app.finish_streaming(
            Some(Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(4_000),
                cache_read_tokens: TokenCount::new(95_000),
                cache_write_tokens: TokenCount::new(5_000),
            }),
            None,
        );

        let system_msgs: Vec<_> = app.messages[before..]
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .collect();
        assert!(
            system_msgs.is_empty(),
            "normal growth should not produce system messages, got: {system_msgs:?}"
        );
    }
}
