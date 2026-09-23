use mush_ai::types::{Dollars, ModelId, ToolCallId, ToolOutcome, Usage};
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::cell::RefCell;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub msg_idx: usize,
    pub content: String,
    pub lang: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Fenced { lang: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub msg_idx: usize,
    pub content: String,
    pub kind: BlockKind,
}

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

#[derive(Debug, Clone)]
pub struct LineCache {
    pub width: u16,
    pub show_usage: bool,
    pub detail: ChatDetail,
    pub pulse: usize,
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
    pub agent_from: Option<String>,
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
            agent_from: self.agent_from.clone(),
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
            agent_from: None,
            line_cache: RefCell::new(None),
        }
    }

    pub fn invalidate(&mut self) {
        self.line_cache.get_mut().take();
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

#[derive(Debug, Clone)]
pub struct ImageRenderArea {
    pub msg_idx: usize,
    pub tc_idx: usize,
    pub area: Rect,
}

#[derive(Debug, Clone)]
pub struct MessageRowRange {
    pub msg_idx: usize,
    pub start: u16,
    pub end: u16,
}
