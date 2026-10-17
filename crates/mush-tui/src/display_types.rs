use mush_ai::types::{Dollars, ModelId, ToolCallId, ToolOutcome, Usage};
use ratatui::text::Line;
use std::cell::RefCell;

#[derive(Debug, Clone, Default)]
pub struct Messages(Vec<DisplayMessage>);

impl std::ops::Deref for Messages {
    type Target = [DisplayMessage];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Vec<DisplayMessage>> for Messages {
    fn from(messages: Vec<DisplayMessage>) -> Self {
        Self(messages)
    }
}

impl FromIterator<DisplayMessage> for Messages {
    fn from_iter<I: IntoIterator<Item = DisplayMessage>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Messages {
    pub fn push(&mut self, message: DisplayMessage) {
        self.0.push(message);
    }

    pub fn insert(&mut self, index: usize, message: DisplayMessage) {
        self.0.insert(index, message);
    }

    pub fn remove(&mut self, index: usize) -> DisplayMessage {
        self.0.remove(index)
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn retain(&mut self, keep: impl FnMut(&DisplayMessage) -> bool) {
        self.0.retain(keep);
    }

    pub fn get_mut(&mut self, index: usize) -> Option<MessageMut<'_>> {
        self.0.get_mut(index).map(MessageMut)
    }

    pub fn last_mut(&mut self) -> Option<MessageMut<'_>> {
        self.0.last_mut().map(MessageMut)
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = MessageMut<'_>> {
        self.0.iter_mut().map(MessageMut)
    }
}

pub struct MessageMut<'a>(&'a mut DisplayMessage);

impl std::ops::Deref for MessageMut<'_> {
    type Target = DisplayMessage;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl std::ops::DerefMut for MessageMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl Drop for MessageMut<'_> {
    fn drop(&mut self) {
        self.0.invalidate();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatDetail {
    Overview,
    #[default]
    Details,
    All,
}

impl ChatDetail {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Overview => Self::Details,
            Self::Details => Self::All,
            Self::All => Self::Overview,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Details => "details",
            Self::All => "all output",
        }
    }

    #[must_use]
    pub const fn shows_thinking(self) -> bool {
        !matches!(self, Self::Overview)
    }

    #[must_use]
    pub const fn shows_diffs(self) -> bool {
        !matches!(self, Self::Overview)
    }

    #[must_use]
    pub const fn shows_full_output(self) -> bool {
        matches!(self, Self::All)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DetailView {
    thinking: bool,
    full_output: bool,
    diffs: bool,
}

impl DetailView {
    #[must_use]
    pub const fn shows_thinking(self) -> bool {
        self.thinking
    }

    #[must_use]
    pub const fn shows_diffs(self) -> bool {
        self.diffs
    }

    #[must_use]
    pub const fn shows_full_output(self) -> bool {
        self.full_output
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DetailSensitivity(DetailView);

impl DetailSensitivity {
    pub const EVERYTHING: Self = Self(DetailView {
        thinking: true,
        full_output: true,
        diffs: true,
    });

    #[must_use]
    pub fn of(message: &DisplayMessage) -> Self {
        let tools = &message.tool_calls;
        Self(DetailView {
            thinking: tools.iter().any(|tc| tc.bash_done.is_some()),
            full_output: message.thinking.is_some()
                || !tools.is_empty()
                || message.injected.is_some(),
            diffs: tools
                .iter()
                .any(|tc| tc.cell.as_ref().is_some_and(|cell| !cell.diffs.is_empty())),
        })
    }

    #[must_use]
    pub const fn view(self, detail: ChatDetail) -> DetailView {
        DetailView {
            thinking: self.0.thinking && detail.shows_thinking(),
            full_output: self.0.full_output && detail.shows_full_output(),
            diffs: self.0.diffs && detail.shows_diffs(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LineCache {
    pub width: u16,
    pub show_usage: bool,
    pub sensitivity: DetailSensitivity,
    pub view: DetailView,
    pub pulse: usize,
    pub highlighted: bool,
    pub lines: Vec<Line<'static>>,
}

#[derive(Debug)]
pub struct DisplayMessage {
    pub role: MessageRole,
    pub content: String,
    pub tool_calls: Vec<DisplayToolCall>,
    pub thinking: Option<String>,
    pub thinking_expanded: bool,
    pub usage: Option<Usage>,
    pub cost: Option<Dollars>,
    pub model_id: Option<ModelId>,
    pub queued: bool,
    pub images: Vec<Vec<u8>>,
    pub injected: Option<crate::injected::Injected>,
    pub expanded: bool,
    pub(crate) line_cache: RefCell<Option<LineCache>>,
}

impl Clone for DisplayMessage {
    fn clone(&self) -> Self {
        Self {
            role: self.role.clone(),
            content: self.content.clone(),
            tool_calls: self.tool_calls.clone(),
            thinking: self.thinking.clone(),
            thinking_expanded: self.thinking_expanded,
            usage: self.usage,
            cost: self.cost,
            model_id: self.model_id.clone(),
            queued: self.queued,
            images: self.images.clone(),
            injected: self.injected.clone(),
            expanded: self.expanded,
            line_cache: RefCell::new(None),
        }
    }
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
            images: Vec::new(),
            injected: None,
            expanded: false,
            line_cache: RefCell::new(None),
        }
    }

    pub fn invalidate(&mut self) {
        self.line_cache.get_mut().take();
    }

    #[must_use]
    pub fn has_highlighting(&self) -> bool {
        !self.tool_calls.is_empty()
            || self.content.contains("```")
            || self
                .injected
                .as_ref()
                .is_some_and(|injected| injected.body.contains("```"))
    }

    #[must_use]
    pub fn text_bytes(&self) -> usize {
        let tools: usize = self
            .tool_calls
            .iter()
            .map(|tc| tc.summary.len() + tc.output.as_ref().map_or(0, String::len))
            .sum();
        self.content.len() + self.thinking.as_ref().map_or(0, String::len) + tools
    }

    #[must_use]
    pub fn is_live(&self) -> bool {
        self.queued
            || self
                .tool_calls
                .iter()
                .any(|tc| tc.status == ToolCallStatus::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Event(mush_ai::types::EventKind),
    Injected,
    BashDone,
    Status { error: bool },
}

#[derive(Debug, Clone)]
pub struct DisplayToolCall {
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub summary: String,
    pub status: ToolCallStatus,
    pub output: Option<String>,
    pub image_data: Option<Vec<u8>>,
    pub batch: u32,
    pub bash_done: Option<mush_tools::kernel_jobs::BashDone>,
    pub cell: Option<mush_tools::ipython::CellDetails>,
}

impl DisplayToolCall {
    #[must_use]
    pub fn new(
        tool_call_id: ToolCallId,
        name: String,
        summary: String,
        status: ToolCallStatus,
        batch: u32,
    ) -> Self {
        Self {
            tool_call_id,
            name,
            summary,
            status,
            output: None,
            image_data: None,
            batch,
            bash_done: None,
            cell: None,
        }
    }
}

pub(crate) fn batch_sub_call_id(batch_call_id: &ToolCallId, index: usize) -> ToolCallId {
    ToolCallId::from(mush_agent::exec_context::batch_sub_exec_id(
        batch_call_id.as_str(),
        index,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Done,
    Error,
}

impl From<ToolOutcome> for ToolCallStatus {
    fn from(outcome: ToolOutcome) -> Self {
        if outcome.is_error() {
            Self::Error
        } else {
            Self::Done
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActiveToolState {
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub summary: String,
    pub live_output: Option<String>,
    pub status: ToolCallStatus,
    pub output: Option<String>,
}

impl ActiveToolState {
    #[must_use]
    pub fn running(tool_call_id: ToolCallId, name: String, summary: String) -> Self {
        Self {
            tool_call_id,
            name,
            summary,
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        }
    }
}
