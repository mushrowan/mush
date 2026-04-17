//! display-oriented tui types

use mush_ai::types::{Dollars, ModelId, ToolCallId, Usage};
use ratatui::layout::Rect;

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
    /// raw image bytes attached to user messages (for inline rendering)
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
