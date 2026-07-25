//! app state and event loop
//!
//! the app holds all TUI state: messages being displayed, current input,
//! streaming status, and scroll position.

use mush_ai::types::*;
use mush_session::SessionMeta;
use std::rc::Rc;

pub use crate::app_event::{AppEvent, AppMode};
pub use crate::app_state::{
    CachedStatusBar, CompactionRecoveryAction, CompactionRecoveryState, CompletionState,
    InteractionState, NavigationState, RenderState, SearchState,
};
pub use crate::batch_output::{
    BatchSection, parse_batch_output, truncate_output, truncate_output_large,
};
pub use crate::display_types::{
    ActiveToolState, Block, BlockKind, CodeBlock, DisplayMessage, DisplayToolCall, ImageRenderArea,
    MessageRole, MessageRowRange, ToolCallStatus,
};
pub use crate::input_buffer::{IMAGE_PLACEHOLDER, InputBuffer, PendingImage};
pub use crate::prompt_cache::{
    BustReason, CACHE_COLD_DISPLAY_SECS, CACHE_WARN_SECS, CacheAnomaly, CacheBustDiagnostic,
    CacheTimer, CallConfig, ContextManagementMode, TokenStats, cache_ttl_secs,
    cache_warning_display, detect_cache_anomalies, direct_route_serving_provider,
    dump_cache_bust_diagnostic, resolve_cache_ttl_secs,
};
pub use crate::session_picker::{SessionPickerState, SessionScope, filtered_sessions};
pub use crate::slash_menu::{ModelCompletion, SlashCommand, SlashMenuState};
pub use crate::streaming::StreamingState;

use crate::slash_menu::{TabState, filter_command_matches};

#[cfg(test)]
thread_local! {
    static BLOCKS_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BLOCKS_COMPUTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_blocks_call_count() {
    BLOCKS_CALLS.with(|calls| calls.set(0));
    BLOCKS_COMPUTES.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn blocks_call_count() -> usize {
    BLOCKS_CALLS.with(std::cell::Cell::get)
}

/// number of times the block list was actually recomputed (cache miss),
/// as opposed to served from the cache. drives the caching regression test
#[cfg(test)]
pub(crate) fn blocks_compute_count() -> usize {
    BLOCKS_COMPUTES.with(std::cell::Cell::get)
}

/// target cadence for streaming text and shimmer updates.
pub(crate) const ANIMATION_FRAME_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(33);

/// ticks in one full unread-flash cycle (~1 second at 60fps)
const UNREAD_FLASH_CYCLE: u8 = 60;

/// ticks the flash indicator stays "on" within each cycle
const UNREAD_FLASH_ON: u8 = 30;

/// default lines scrolled per j/k keystroke in scroll mode. overridable
/// via `scroll_lines` in `config.toml`
pub const DEFAULT_SCROLL_LINES: u16 = 3;

/// truncate a tool's output for the preview/panel display.
///
/// edit diffs are preserved whole because they're bounded by the size of
/// the change under review and the message viewport scrolls naturally
/// for the rest. every other tool gets the generous single-tool cap so
/// a runaway bash log or massive grep doesn't paint thousands of rows
fn preview_for_tool(name: &str, output: &str) -> String {
    if name.eq_ignore_ascii_case("edit") {
        output.to_owned()
    } else {
        truncate_output_large(output)
    }
}

fn context_management_mode_for_model_id(
    model_id: &str,
    request_used_context_management: Option<bool>,
) -> ContextManagementMode {
    if model_id.contains("codex") {
        ContextManagementMode::SessionManaged
    } else if (model_id.contains("claude-") && request_used_context_management.unwrap_or(false))
        || model_id.contains("gpt-")
        || model_id.starts_with('o')
    {
        ContextManagementMode::ProviderManaged
    } else {
        ContextManagementMode::LocalMonotonic
    }
}

/// controls how thinking text is displayed
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySelection {
    pub text: String,
    pub status: String,
}

/// pick the next thinking level. when `supported` is non-empty, walks
/// the curated list (cheap → expensive) wrapping around. when empty,
/// falls back to the legacy hardcoded cycle so uncurated models still
/// respond to ctrl+t. unknown / out-of-list current levels reset to
/// the first supported entry instead of stalling
#[must_use]
pub fn next_thinking_level(current: &ThinkingLevel, supported: &[ThinkingLevel]) -> ThinkingLevel {
    if supported.is_empty() {
        return match current.normalize_visible() {
            ThinkingLevel::Off => ThinkingLevel::Low,
            ThinkingLevel::Low => ThinkingLevel::Medium,
            ThinkingLevel::Medium => ThinkingLevel::High,
            ThinkingLevel::High => ThinkingLevel::Xhigh,
            ThinkingLevel::Xhigh | ThinkingLevel::Minimal | ThinkingLevel::Other(_) => {
                ThinkingLevel::Off
            }
        };
    }
    let normalised = current.normalize_visible();
    match supported.iter().position(|l| *l == normalised) {
        Some(i) => supported[(i + 1) % supported.len()].clone(),
        None => supported[0].clone(),
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
    /// steady animation frame counter
    tick_count: u64,
    /// current thinking level
    pub thinking_level: ThinkingLevel,
    /// the active model's curated thinking levels (drives the cycler).
    /// empty when the model hasn't been curated; the cycler then falls
    /// back to the legacy hardcoded walk
    pub supported_thinking_levels: Vec<ThinkingLevel>,
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
    /// provider id that produced `oauth_usage`.
    /// prevents stale claude usage bars showing while a codex/openai pane is active
    pub oauth_usage_provider: Option<String>,
    /// in-progress paste-the-code oauth flow started via /login when no
    /// loopback callback server could be bound.
    /// consumed when /login-complete fires with the authorization code
    pub pending_oauth: Option<PendingOAuth>,
    /// browser oauth flow waiting on its loopback callback. the runner
    /// drains this each frame, so no /login-complete is needed
    pub pending_browser_login: Option<crate::browser_login::PendingBrowserLogin>,
    /// state for the /settings floating menu (only populated when
    /// interaction.mode == AppMode::Settings)
    pub settings_menu: Option<crate::settings::SettingsMenuState>,
    /// colour theme for all widgets
    pub theme: crate::theme::Theme,
    /// lines scrolled per j/k keystroke in scroll mode
    pub scroll_lines: u16,
    /// keybind map: action → key combination(s)
    pub keymap: crate::keybinds::KeyMap,
    /// armed state for ctrl+c quit confirmation. set on first ctrl+c
    /// when the prompt is empty and idle, cleared by any other key
    pub confirm_quit_pending: bool,
    /// true while a /compact or /fork-compact LLM call is running on
    /// a background task. drives the busy label so the
    /// user sees compaction as a discrete busy state and routes new
    /// submits into the queued-message bubble instead of clobbering
    /// the input buffer
    pub compacting: bool,
    /// model used for explicit and automatic compaction, or None when
    /// compaction uses the active pane model
    pub compaction_model_id: Option<ModelId>,
}

mod modals;
mod scroll;
mod selection;
mod streaming;

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
            tick_count: 0,
            thinking_level: ThinkingLevel::Off,
            supported_thinking_levels: Vec::new(),
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
            oauth_usage_provider: None,
            pending_oauth: None,
            pending_browser_login: None,
            settings_menu: None,
            theme: crate::theme::Theme::default(),
            scroll_lines: DEFAULT_SCROLL_LINES,
            keymap: crate::keybinds::KeyMap::default(),
            confirm_quit_pending: false,
            compacting: false,
            compaction_model_id: None,
        }
    }

    /// advance streaming text only. token events may arrive in bursts,
    /// so keep them from changing the visual animation phase
    pub fn tick_streaming_text(&mut self) {
        if self.stream.active {
            self.stream.advance_typewriter();
        }
    }

    /// advance visible animations on a steady timer
    pub fn tick_animation(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        self.tick_streaming_text();
    }

    #[cfg(test)]
    pub fn tick(&mut self) {
        self.tick_animation();
    }

    /// reset the scroll anchor on a real terminal resize.
    ///
    /// the baseline anchors viewport position to a content line plus a
    /// snapshot of the viewport height, so streaming-time line growth
    /// and small status-bar wraps don't shift the user's view. but a
    /// real terminal resize is a much larger change: the new vis
    /// makes the cached baseline_vis stale, and the negative
    /// compensation that ensues clamps `effective_offset` to 0,
    /// swallowing the first N keypresses worth of scrolling. clearing
    /// the baseline lets the next render re-pin against the new
    /// dimensions, so subsequent k/j presses move the viewport
    /// immediately. status-bar height jitter (vis ±1 between frames
    /// from cache countdown text width changing, etc) is unaffected
    /// because it doesn't go through this path.
    ///
    /// `prev_content_lines` is also cleared because it gets fed into
    /// the next baseline capture (in `update_scroll_baseline`'s
    /// `(_, 0)` arm). without this reset the post-resize baseline
    /// would anchor against a line count computed under the OLD wrap
    /// width, producing a wrong (often negative) compensation that
    /// clamps `effective_offset` to 0 and locks the viewport at
    /// max_scroll - the "j stops scrolling down after a resize"
    /// symptom. one frame of zero-compensation rendering follows the
    /// resize while the new total propagates, then anchoring resumes
    /// correctly
    pub fn notify_resize(&mut self) {
        let prior_baseline = self.render_state.scroll_baseline.get();
        let prior_baseline_vis = self.render_state.scroll_baseline_vis.get();
        let prior_prev_content_lines = self.render_state.prev_content_lines.get();
        self.render_state.scroll_baseline.set(0);
        self.render_state.scroll_baseline_vis.set(0);
        self.render_state.prev_content_lines.set(0);
        tracing::info!(
            target: "mush_tui::resize",
            scroll_offset = self.scroll_offset,
            prior_baseline,
            prior_baseline_vis,
            prior_prev_content_lines,
            "notify_resize cleared scroll baseline"
        );
    }

    /// whether the unread flash indicator is in the "on" phase
    /// cycles at ~1hz
    pub fn unread_flash_on(&self) -> bool {
        self.tick_count % u64::from(UNREAD_FLASH_CYCLE) < u64::from(UNREAD_FLASH_ON)
    }

    /// phase offset for shimmer-styled labels. advances every animation
    /// frame, with the palette providing enough intermediate colours to
    /// keep movement slow and smooth
    pub fn shimmer_phase(&self) -> usize {
        self.tick_count as usize
    }

    /// whether this pane currently has time-driven visual animation.
    /// running tools animate the status-bar activity label even when the
    /// provider stream is parked waiting for tool completion
    pub fn has_visible_animation(&self) -> bool {
        self.compacting
            || self.stream.active
            || self.navigation.has_unread
            || self.has_running_tool()
    }

    /// whether a draw is useful even when no event arrived. this keeps
    /// wall-clock UI affordances fresh without forcing a maintenance
    /// redraw every idle second
    pub fn needs_periodic_redraw(&self, cache_timer_enabled: bool) -> bool {
        self.has_visible_animation()
            || (cache_timer_enabled && self.cache.remaining_secs().is_some())
    }

    /// whether the agent is currently active (streaming or executing tools)
    pub fn is_busy(&self) -> bool {
        self.compacting || self.stream.active || self.has_running_tool()
    }

    /// short label describing what the agent is currently doing, suitable
    /// for the status bar. returns `None` when idle. tool execution wins
    /// over streaming because tool calls are concrete activity even when
    /// the assistant message is still flagged active during the gap
    /// between message_end and the next turn_start
    pub fn activity_label(&self) -> Option<String> {
        if self.compacting {
            return Some("compacting…".into());
        }
        let running: Vec<&str> = self.running_tool_names().collect();
        if !running.is_empty() {
            let first = running[0];
            let all_same = running.iter().all(|n| *n == first);
            return Some(if all_same {
                format!("using {first}…")
            } else {
                format!("using {} tools…", running.len())
            });
        }
        if self.stream.active {
            return Some(if self.stream.text.is_empty() {
                "thinking…".into()
            } else {
                "outputting…".into()
            });
        }
        None
    }

    fn has_running_tool(&self) -> bool {
        self.running_tool_names().next().is_some()
    }

    fn running_tool_names(&self) -> impl Iterator<Item = &str> {
        self.active_tools
            .iter()
            .filter(|t| t.status == ToolCallStatus::Running)
            .map(|t| t.name.as_str())
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

    /// add a queued steering message carrying attached images, so the UI
    /// reflects the pending image(s) on the queued bubble
    pub fn push_queued_message_with_images(
        &mut self,
        text: impl Into<String>,
        images: Vec<Vec<u8>>,
    ) {
        let mut msg = DisplayMessage::new(MessageRole::User, text);
        msg.queued = true;
        msg.images = images;
        self.messages.push(msg);
        self.scroll_offset = 0;
    }

    /// remove the last queued steering message from display, return its text
    pub fn pop_last_queued_message(&mut self) -> Option<String> {
        let idx = self.messages.iter().rposition(|m| m.queued)?;
        let msg = self.messages.remove(idx);
        Some(msg.content)
    }

    /// whether any queued steering messages are currently visible
    #[must_use]
    pub fn has_queued_messages(&self) -> bool {
        self.messages.iter().any(|m| m.queued)
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
        // suppress ghost when the input is itself an exact slash command name,
        // otherwise Enter would accept a longer sibling (e.g. /login submitting
        // /login-complete) instead of running the typed command
        if let Some(rest) = text.strip_prefix('/')
            && self
                .completion
                .slash_commands
                .iter()
                .any(|c| c.name == rest)
        {
            return None;
        }
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

    /// clear display state without touching stats (for rebuild_display).
    /// the conversation might be getting rewritten in-place (compact,
    /// branch, undo) where the cumulative session totals are still
    /// correct; only the rendered list of messages and scroll position
    /// need to reset.
    pub(crate) fn clear_display(&mut self) {
        self.messages.clear();
        self.stream = StreamingState::new();
        self.scroll_offset = 0;
        self.input.images.clear();
    }

    /// clear all messages AND reset cumulative stats (for /clear, /new).
    /// use `clear_display` if stats should be preserved
    pub fn clear_messages(&mut self) {
        self.clear_display();
        self.stats.reset();
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

    /// cycle to the next thinking level. when the active model advertises
    /// a curated `supported_thinking_levels` list, walk it (cheap →
    /// expensive, wrapping around). otherwise fall back to the legacy
    /// hardcoded cycle so uncurated models still respond to ctrl+t
    pub fn cycle_thinking_level(&mut self) {
        self.thinking_level =
            next_thinking_level(&self.thinking_level, &self.supported_thinking_levels);
    }

    /// route a bracketed-paste payload to whatever input field is
    /// currently active. without this, every paste lands in the main
    /// prompt buffer even when an overlay (like the /login api-key
    /// entry prompt) owns keyboard focus, leaking secrets into the
    /// visible message box behind the overlay
    pub fn handle_paste(&mut self, text: &str) {
        if let Some(prompt) = self
            .interaction
            .login_picker
            .as_mut()
            .and_then(|p| p.entry.as_mut())
        {
            prompt.buffer.push_str(text);
            return;
        }
        self.input.insert_str(text);
    }
}

#[cfg(test)]
mod tests;
