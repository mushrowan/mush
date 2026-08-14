use mush_ai::types::{Dollars, ModelId, ToolCallId, ToolOutcome, Usage};
use ratatui::layout::Rect;

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

#[derive(Debug, Clone)]
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
        }
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
}

#[derive(Debug, Clone)]
pub struct DisplayToolCall {
    pub name: String,
    pub summary: String,
    pub status: ToolCallStatus,
    pub output_preview: Option<String>,
    pub image_data: Option<Vec<u8>>,
    pub batch: u32,
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
