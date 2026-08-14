use mush_ai::types::*;
use mush_session::SessionMeta;

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

pub(crate) const ANIMATION_FRAME_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(33);

const UNREAD_FLASH_CYCLE: u8 = 60;

const UNREAD_FLASH_ON: u8 = 30;

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
    Hidden,
    Collapse,
    #[default]
    Expanded,
}

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

pub struct App {
    pub messages: Vec<DisplayMessage>,
    pub stream: StreamingState,
    pub input: InputBuffer,
    pub model_id: ModelId,
    pub stats: TokenStats,
    pub should_quit: bool,
    pub status: Option<String>,
    pub active_tools: Vec<ActiveToolState>,
    tick_count: u64,
    pub thinking_level: ThinkingLevel,
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    pub thinking_display: ThinkingDisplay,
    pub interaction: InteractionState,
    pub completion: CompletionState,
    pub navigation: NavigationState,
    pub render_state: RenderState,
    pub cwd: String,
    pub pane_info: Option<(u16, u16)>,
    pub background_alert: Option<String>,
    pub cache: CacheTimer,
    current_tool_batch: u32,
    display_generation: u64,
    pub viewport: crate::screen::Viewport,
    pub oauth_usage: Option<mush_ai::oauth::usage::OAuthUsage>,
    pub oauth_usage_provider: Option<String>,
    pub pending_oauth: Option<PendingOAuth>,
    pub pending_browser_login: Option<crate::browser_login::PendingBrowserLogin>,
    pub settings_menu: Option<crate::settings::SettingsMenuState>,
    pub theme: crate::theme::Theme,
    pub keymap: crate::keybinds::KeyMap,
    pub confirm_quit_pending: bool,
    pub compacting: bool,
    pub compaction_model_id: Option<ModelId>,
}

mod modals;
mod streaming;

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
            display_generation: 0,
            viewport: crate::screen::Viewport::default(),
            oauth_usage: None,
            oauth_usage_provider: None,
            pending_oauth: None,
            pending_browser_login: None,
            settings_menu: None,
            theme: crate::theme::Theme::default(),
            keymap: crate::keybinds::KeyMap::default(),
            confirm_quit_pending: false,
            compacting: false,
            compaction_model_id: None,
        }
    }

    pub fn tick_streaming_text(&mut self) {
        if self.stream.active {
            self.stream.advance_typewriter();
        }
    }

    pub fn tick_animation(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        self.tick_streaming_text();
    }

    #[cfg(test)]
    pub fn tick(&mut self) {
        self.tick_animation();
    }

    pub fn unread_flash_on(&self) -> bool {
        self.tick_count % u64::from(UNREAD_FLASH_CYCLE) < u64::from(UNREAD_FLASH_ON)
    }

    pub fn shimmer_phase(&self) -> usize {
        self.tick_count as usize
    }

    pub fn has_visible_animation(&self) -> bool {
        self.compacting
            || self.stream.active
            || self.navigation.has_unread
            || self.has_running_tool()
    }

    pub fn needs_periodic_redraw(&self, cache_timer_enabled: bool) -> bool {
        self.has_visible_animation()
            || (cache_timer_enabled && self.cache.remaining_secs().is_some())
    }

    pub fn is_busy(&self) -> bool {
        self.compacting || self.stream.active || self.has_running_tool()
    }

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

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages
            .push(DisplayMessage::new(MessageRole::User, text));
    }

    pub fn push_user_message_with_images(&mut self, text: impl Into<String>, images: Vec<Vec<u8>>) {
        let mut msg = DisplayMessage::new(MessageRole::User, text);
        msg.images = images;
        self.messages.push(msg);
    }

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

    pub fn push_queued_message(&mut self, text: impl Into<String>) {
        self.messages.push(DisplayMessage {
            queued: true,
            ..DisplayMessage::new(MessageRole::User, text)
        });
    }

    pub fn push_queued_message_with_images(
        &mut self,
        text: impl Into<String>,
        images: Vec<Vec<u8>>,
    ) {
        let mut msg = DisplayMessage::new(MessageRole::User, text);
        msg.queued = true;
        msg.images = images;
        self.messages.push(msg);
    }

    pub fn pop_last_queued_message(&mut self) -> Option<String> {
        let idx = self.messages.iter().rposition(|m| m.queued)?;
        let msg = self.messages.remove(idx);
        Some(msg.content)
    }

    #[must_use]
    pub fn has_queued_messages(&self) -> bool {
        self.messages.iter().any(|m| m.queued)
    }

    pub fn input_char(&mut self, c: char) {
        self.completion.tab_state = None;
        self.input.insert_char(c);
    }

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
        if self.interaction.search.matches.is_empty() {
            self.interaction.search.selected = 0;
        } else if self.interaction.search.selected >= self.interaction.search.matches.len() {
            self.interaction.search.selected = self.interaction.search.matches.len() - 1;
        }
    }

    pub fn ghost_text(&self) -> Option<&str> {
        if self.completion.tab_state.is_some() {
            return None;
        }
        if self.input.cursor != self.input.text.len() || self.input.text.is_empty() {
            return None;
        }
        let text = self.input.text.as_str();
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

    pub(crate) fn clear_display(&mut self) {
        self.display_generation = self.display_generation.wrapping_add(1);
        self.messages.clear();
        self.stream = StreamingState::new();
        self.input.images.clear();
    }

    pub fn clear_messages(&mut self) {
        self.clear_display();
        self.stats.reset();
    }

    pub fn push_system_message(&mut self, text: impl Into<String>) {
        self.messages
            .push(DisplayMessage::new(MessageRole::System, text));
    }

    #[must_use]
    pub fn display_generation(&self) -> u64 {
        self.display_generation
    }

    #[must_use]
    pub fn commit_point(&self) -> usize {
        let live_turn = self
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::User && !m.queued)
            .unwrap_or(self.messages.len());
        let unsettled = self
            .messages
            .iter()
            .position(DisplayMessage::is_live)
            .unwrap_or(self.messages.len());
        live_turn.min(unsettled)
    }

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

    pub fn cycle_thinking_level(&mut self) {
        self.thinking_level =
            next_thinking_level(&self.thinking_level, &self.supported_thinking_levels);
    }

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
