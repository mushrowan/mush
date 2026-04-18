//! app state and event loop
//!
//! the app holds all TUI state: messages being displayed, current input,
//! streaming status, and scroll position.

use mush_ai::types::*;
use mush_session::SessionMeta;
use throbber_widgets_tui::ThrobberState;

pub use crate::app_event::{AppEvent, AppMode};
pub use crate::app_state::{
    CompletionState, InteractionState, NavigationState, RenderState, SearchState,
};
pub use crate::batch_output::{BatchSection, parse_batch_output, truncate_output};
pub use crate::cache::{
    BustReason, CACHE_COLD_DISPLAY_SECS, CACHE_WARN_SECS, CacheAnomaly, CacheBustDiagnostic,
    CacheTimer, CallConfig, TokenStats, cache_ttl_secs, detect_cache_anomalies,
    dump_cache_bust_diagnostic,
};
pub use crate::display_types::{
    ActiveToolState, CodeBlock, DisplayMessage, DisplayToolCall, ImageRenderArea, MessageRole,
    MessageRowRange, ToolCallStatus,
};
pub use crate::input_buffer::{IMAGE_PLACEHOLDER, InputBuffer, PendingImage};
pub use crate::session_picker::{SessionPickerState, SessionScope, filtered_sessions};
pub use crate::slash_menu::{ModelCompletion, SlashCommand, SlashMenuState};
pub use crate::streaming::StreamingState;

use crate::slash_menu::{TabState, filter_command_matches, filter_model_matches};

/// tick() runs at ~60fps, divide by this to get spinner update rate (~8fps)
const TICK_DIVISOR: u8 = 8;

/// ticks in one full unread-flash cycle (~1 second at 60fps)
const UNREAD_FLASH_CYCLE: u8 = 60;

/// ticks the flash indicator stays "on" within each cycle
const UNREAD_FLASH_ON: u8 = 30;

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

/// what j/k navigates in scroll mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollUnit {
    /// navigate between messages
    Message,
    /// navigate between fenced code blocks
    #[default]
    Block,
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
    /// modal and prompt-related state
    pub interaction: InteractionState,
    /// slash completion and tab cycling state
    pub completion: CompletionState,
    /// selection and scroll navigation state
    pub navigation: NavigationState,
    /// render caches and geometry from the last frame
    pub render_state: RenderState,
    /// working directory (with ~ for home)
    pub cwd: String,
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
    /// in-progress oauth flow started via /login.
    /// consumed when /login-complete fires with the authorization code
    pub pending_oauth: Option<PendingOAuth>,
    /// state for the /settings floating menu (only populated when
    /// interaction.mode == AppMode::Settings)
    pub settings_menu: Option<crate::settings::SettingsMenuState>,
    /// colour theme for all widgets
    pub theme: crate::theme::Theme,
}

/// state captured when /login starts an oauth flow, consumed when the user
/// provides the authorization code via /login-complete
#[derive(Debug, Clone)]
pub struct PendingOAuth {
    pub provider_id: String,
    pub provider_name: String,
    pub pkce: mush_ai::oauth::PkceChallenge,
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
            interaction: InteractionState::default(),
            completion: CompletionState::default(),
            navigation: NavigationState::default(),
            render_state: RenderState::new(),
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
            pane_info: None,
            background_alert: None,
            cache: CacheTimer::new(300),
            current_tool_batch: 0,
            oauth_usage: None,
            pending_oauth: None,
            settings_menu: None,
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

    /// add a user message with attached images to the display
    pub fn push_user_message_with_images(&mut self, text: impl Into<String>, images: Vec<Vec<u8>>) {
        let mut msg = DisplayMessage::new(MessageRole::User, text);
        msg.images = images;
        self.messages.push(msg);
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
        // don't reset scroll_offset here: push_user_message() already does it
        // when the user sends a message. resetting here would yank the user
        // to the bottom if streaming starts in other contexts (delegation,
        // resume, etc) while they're scrolled up
    }

    /// append text delta to the current stream
    pub fn push_text_delta(&mut self, delta: &str) {
        self.stream.push_text(delta);
    }

    /// append thinking delta to the current stream
    pub fn push_thinking_delta(&mut self, delta: &str) {
        self.stream.push_thinking(delta);
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
            let prev_usage_snapshot = self.stats.prev_usage().copied();
            let prev_config_snapshot = self.stats.prev_call_config().cloned();
            let prev_context_snapshot = self.stats.context_tokens;
            let curr_config = CallConfig {
                model_id: self.model_id.to_string(),
                thinking_level: format!("{:?}", self.thinking_level),
                // effort is derived from (model, thinking_level) today; a
                // standalone effort field will land with the effort enum split
                effort: None,
            };
            let anomalies = self
                .stats
                .update_with_config(u, cost, Some(curr_config.clone()));
            for anomaly in &anomalies {
                let msg_opt: Option<String> = match anomaly {
                    CacheAnomaly::ContextDecrease { prev, curr } => Some(format!(
                        "⚠ context decreased: {}k → {}k (delta -{}k) without compact",
                        prev.get() / 1000,
                        curr.get() / 1000,
                        (prev.get() - curr.get()) / 1000,
                    )),
                    CacheAnomaly::CacheBust {
                        prev_cache_read,
                        curr_cache_read,
                        curr_cache_write,
                        reason,
                    } => {
                        // dump diagnostic to disk for post-mortem investigation
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let diag = CacheBustDiagnostic {
                            timestamp: format!("{now}"),
                            secs_since_last_cache_activity: self.cache.elapsed_secs(),
                            cache_ttl_secs: self.cache.ttl_secs,
                            thinking_level: curr_config.thinking_level.clone(),
                            model_id: curr_config.model_id.clone(),
                            effort: curr_config.effort.clone(),
                            bust_reason: reason.clone(),
                            prev_model_id: prev_config_snapshot
                                .as_ref()
                                .map(|c| c.model_id.clone()),
                            prev_thinking_level: prev_config_snapshot
                                .as_ref()
                                .map(|c| c.thinking_level.clone()),
                            prev_effort: prev_config_snapshot
                                .as_ref()
                                .and_then(|c| c.effort.clone()),
                            prev_usage: prev_usage_snapshot,
                            curr_usage: *u,
                            prev_context_tokens: prev_context_snapshot.get(),
                            curr_context_tokens: u.total_input_tokens().get(),
                            session_total_cost: format!("{:.4}", self.stats.total_cost.get()),
                            session_api_calls: self.stats.total_tokens.get()
                                / u.total_tokens().get().max(1),
                        };
                        let dump_note = match dump_cache_bust_diagnostic(&diag) {
                            Some(path) => format!(" (dump: {})", path.display()),
                            None => String::new(),
                        };

                        match reason {
                            BustReason::Unexplained => Some(format!(
                                "⚠ probable cache bust: cache_read {}k → {}k, cache_write {}k (prefix evicted){dump_note}",
                                prev_cache_read.get() / 1000,
                                curr_cache_read.get() / 1000,
                                curr_cache_write.get() / 1000,
                            )),
                            // expected bust from user-initiated config change:
                            // dump for forensics but don't alarm the user
                            _ => None,
                        }
                    }
                };
                if let Some(msg) = msg_opt {
                    self.push_system_message(msg);
                }
            }
        } else if let Some(c) = cost {
            self.stats.total_cost += c;
        }
        if self.scroll_offset > 0 {
            self.navigation.has_unread = true;
        }
    }

    /// insert a character at the cursor (clears tab completion)
    pub fn input_char(&mut self, c: char) {
        self.completion.tab_state = None;
        self.input.insert_char(c);
    }

    /// cycle through tab completions for the current input
    pub fn tab_complete(&mut self) {
        if let Some(ref mut state) = self.completion.tab_state {
            state.index = (state.index + 1) % state.matches.len();
            let replacement = &state.matches[state.index];
            self.input.text = replacement.clone();
            self.input.cursor = self.input.text.len();
            self.input.ensure_cursor_visible();
            return;
        }

        let text = self.input.text.as_str();
        let matches: Vec<String> = if let Some(rest) = text.strip_prefix("/model ") {
            self.completion
                .completions
                .iter()
                .filter(|c| !c.starts_with('/'))
                .filter(|c| c.starts_with(rest))
                .map(|c| format!("/model {c}"))
                .collect()
        } else if text.starts_with('/') {
            self.completion
                .completions
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
        self.completion.tab_state = Some(TabState { matches, index: 0 });
        self.input.text = first;
        self.input.cursor = self.input.text.len();
        self.input.ensure_cursor_visible();
    }

    /// open the slash command completion menu, filtering by current input
    pub fn open_slash_menu(&mut self) {
        let prefix = self.input.text.as_str();

        if let Some(rest) = prefix.strip_prefix("/model ") {
            let model_matches = filter_model_matches(&self.completion.model_completions, rest);
            if model_matches.is_empty() {
                return;
            }

            self.completion.slash_menu = Some(SlashMenuState::for_models(model_matches));
            self.interaction.mode = AppMode::SlashComplete;
            return;
        }

        let matches = filter_command_matches(&self.completion.slash_commands, prefix);
        if matches.is_empty() {
            return;
        }

        self.completion.slash_menu = Some(SlashMenuState::for_commands(matches));
        self.interaction.mode = AppMode::SlashComplete;
    }

    /// update the slash menu filter based on current input
    pub fn update_slash_menu(&mut self) {
        if let Some(ref mut menu) = self.completion.slash_menu {
            let prefix = self.input.text.as_str();

            if let Some(rest) = prefix.strip_prefix("/model ") {
                menu.model_mode = true;
                menu.model_matches = filter_model_matches(&self.completion.model_completions, rest);
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
            menu.matches = filter_command_matches(&self.completion.slash_commands, prefix);
            menu.model_matches.clear();
            menu.selected = menu.selected.min(menu.matches.len().saturating_sub(1));

            if menu.matches.is_empty() {
                self.close_slash_menu();
            }
        }
    }

    /// close the slash menu and return to normal mode
    pub fn close_slash_menu(&mut self) {
        self.completion.slash_menu = None;
        self.interaction.mode = AppMode::Normal;
    }

    /// jump to bottom of conversation and clear unread indicator
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.navigation.has_unread = false;
    }

    /// whether visual selection mode is active (v in scroll mode)
    pub fn has_selection(&self) -> bool {
        self.navigation.selection_anchor.is_some() && self.navigation.selected_message.is_some()
    }

    /// get the inclusive selection range (min..=max), if active
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.navigation.selection_anchor?;
        let cursor = self.navigation.selected_message?;
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    /// find which message is displayed at a given screen row
    ///
    /// uses render metadata (message_row_ranges, render_scroll, message_area)
    /// populated by MessageList during the last render pass
    pub fn message_at_screen_row(&self, row: u16) -> Option<usize> {
        let area = self.render_state.message_area.get();
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let screen_line = self.render_state.render_scroll.get() + (row - area.y);
        let ranges = self.render_state.message_row_ranges.borrow();
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
        let q = self.interaction.search.query.to_lowercase();
        self.interaction.search.matches = if q.is_empty() {
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
        if self.interaction.search.matches.is_empty() {
            self.interaction.search.selected = 0;
        } else if self.interaction.search.selected >= self.interaction.search.matches.len() {
            self.interaction.search.selected = self.interaction.search.matches.len() - 1;
        }
    }

    /// return ghost completion suffix for inline hint (dimmed text after cursor).
    /// only shown when cursor is at end and no active tab cycle.
    pub fn ghost_text(&self) -> Option<&str> {
        if self.completion.tab_state.is_some() {
            return None;
        }
        if self.input.cursor != self.input.text.len() || self.input.text.is_empty() {
            return None;
        }
        let text = self.input.text.as_str();
        let candidate = if let Some(rest) = text.strip_prefix("/model ") {
            self.completion
                .completions
                .iter()
                .filter(|c| !c.starts_with('/'))
                .find(|c| c.starts_with(rest))
                .map(|c| &c[rest.len()..])
        } else if text.starts_with('/') {
            self.completion
                .completions
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
        self.interaction.session_picker = Some(SessionPickerState::new(sessions, cwd));
        self.interaction.mode = AppMode::SessionPicker;
    }

    /// close the session picker
    pub fn close_session_picker(&mut self) {
        self.interaction.session_picker = None;
        self.interaction.mode = AppMode::Normal;
    }

    /// get the currently selected session id (if picker is open)
    pub fn selected_session(&self) -> Option<&SessionMeta> {
        let picker = self.interaction.session_picker.as_ref()?;
        let filtered = filtered_sessions(picker);
        filtered.get(picker.selected).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_buffer::word_boundary_left;
    use crate::streaming::char_prefix;
    use mush_ai::types::ToolOutcome;
    use ratatui::layout::Rect;

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
    fn push_user_message_stores_images() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let images = vec![vec![0u8; 100], vec![1u8; 200]];
        app.push_user_message_with_images("hello", images.clone());
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "hello");
        assert_eq!(app.messages[0].images.len(), 2);
        assert_eq!(app.messages[0].images[0], images[0]);
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
        assert_eq!(app.interaction.mode, AppMode::Normal);

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
        assert_eq!(app.interaction.mode, AppMode::SessionPicker);
        assert!(app.interaction.session_picker.is_some());

        app.close_session_picker();
        assert_eq!(app.interaction.mode, AppMode::Normal);
        assert!(app.interaction.session_picker.is_none());
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

        let picker = app.interaction.session_picker.as_mut().unwrap();
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
        let picker = app.interaction.session_picker.as_ref().unwrap();
        assert_eq!(picker.scope, SessionScope::ThisDir);
        let filtered = filtered_sessions(picker);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title.as_deref(), Some("local session"));

        // all dirs: both sessions
        let picker = app.interaction.session_picker.as_mut().unwrap();
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
            cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Short), false),
            300
        );
        assert_eq!(
            cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Long), false),
            3600
        );
        assert_eq!(
            cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::None), false),
            0
        );
        assert_eq!(cache_ttl_secs(&Provider::Anthropic, None, false), 300); // default = short

        // oauth always gets 1h, regardless of retention setting
        assert_eq!(cache_ttl_secs(&Provider::Anthropic, None, true), 3600);
        assert_eq!(
            cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Short), true),
            3600,
            "oauth should ignore Short retention and use 1h"
        );

        // openrouter / custom: defaults to 300
        assert_eq!(cache_ttl_secs(&Provider::OpenRouter, None, false), 300);
        assert_eq!(
            cache_ttl_secs(&Provider::Custom("xai".into()), None, false),
            300
        );
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
        app.navigation.selection_anchor = Some(3);
        app.navigation.selected_message = Some(1);
        assert!(app.has_selection());
        assert_eq!(app.selection_range(), Some((1, 3)));

        // anchor at 1, cursor at 3 => range (1, 3)
        app.navigation.selection_anchor = Some(1);
        app.navigation.selected_message = Some(3);
        assert_eq!(app.selection_range(), Some((1, 3)));

        // same index => single message range
        app.navigation.selection_anchor = Some(2);
        app.navigation.selected_message = Some(2);
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
        stream.push_text("leftover");
        stream.start();
        assert!(stream.active);
        assert!(stream.text.is_empty());
        assert!(stream.thinking.is_empty());
    }

    #[test]
    fn streaming_state_visible_text_typewriter() {
        let mut stream = StreamingState::new();
        stream.start();
        stream.push_text("hello world");
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
        stream.push_text("answer");
        stream.push_thinking("reasoning");
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
        stream.push_text("just text");
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
    fn is_busy_during_tool_execution() {
        // stream.active is false during tool execution (text streaming
        // ended) but is_busy() should still return true when tools are
        // running, so the throbber keeps animating
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        assert!(!app.stream.active);
        assert!(!app.is_busy());

        // simulate tool running with stream inactive
        app.active_tools
            .push(crate::display_types::ActiveToolState {
                tool_call_id: "test_1".into(),
                name: "bash".into(),
                summary: "running command".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            });

        assert!(
            !app.stream.active,
            "stream should be inactive during tool execution"
        );
        assert!(
            app.is_busy(),
            "is_busy should be true when tools are running"
        );

        // throbber should advance when ticked during tool execution
        let initial = app.throbber_state.clone();
        for _ in 0..TICK_DIVISOR {
            app.tick();
        }
        assert_ne!(
            format!("{:?}", app.throbber_state),
            format!("{initial:?}"),
            "throbber should advance during tool execution"
        );
    }

    #[test]
    fn unread_flash_cycle_works() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.navigation.has_unread = true;
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
        let anomalies = detect_cache_anomalies(Some(&prev), &curr, None, None);
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
        let anomalies = detect_cache_anomalies(Some(&prev), &curr, None, None);
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
        let anomalies = detect_cache_anomalies(Some(&prev), &curr, None, None);
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
        let anomalies = detect_cache_anomalies(None, &curr, None, None);
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
    fn cache_bust_diagnostic_writes_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let bust_dir = dir.path().join("cache-busts");

        let diag = CacheBustDiagnostic {
            timestamp: "1722470400".into(),
            secs_since_last_cache_activity: Some(42),
            cache_ttl_secs: 300,
            thinking_level: "High".into(),
            model_id: "claude-sonnet-4-20250514".into(),
            effort: None,
            bust_reason: BustReason::Unexplained,
            prev_model_id: None,
            prev_thinking_level: None,
            prev_effort: None,
            prev_usage: Some(Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::new(90_000),
                cache_write_tokens: TokenCount::new(5_000),
            }),
            curr_usage: Usage {
                input_tokens: TokenCount::new(5_000),
                output_tokens: TokenCount::new(3_000),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::new(100_000),
            },
            prev_context_tokens: 100_000,
            curr_context_tokens: 105_000,
            session_total_cost: "1.2345".into(),
            session_api_calls: 7,
        };

        // test the dump function directly using internal helper
        std::fs::create_dir_all(&bust_dir).unwrap();
        let json = serde_json::to_string_pretty(&diag).unwrap();
        let path = bust_dir.join("bust-test.json");
        std::fs::write(&path, &json).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cache_ttl_secs"], 300);
        assert_eq!(parsed["secs_since_last_cache_activity"], 42);
        assert_eq!(parsed["thinking_level"], "High");
        assert_eq!(parsed["prev_usage"]["cache_read_tokens"], 90_000);
        assert_eq!(parsed["curr_usage"]["cache_write_tokens"], 100_000);
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
