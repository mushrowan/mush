//! app state and event loop
//!
//! the app holds all TUI state: messages being displayed, current input,
//! streaming status, and scroll position.

use mush_ai::types::*;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Done,
    Error,
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
    pub model_id: String,
    /// total cost so far
    pub total_cost: f64,
    /// total tokens used
    pub total_tokens: u64,
    /// whether we should quit
    pub should_quit: bool,
    /// status message (bottom bar)
    pub status: Option<String>,
    /// current tool being executed
    pub active_tool: Option<String>,
    /// spinner state for animations
    pub throbber_state: ThrobberState,
}

impl App {
    pub fn new(model_id: String) -> Self {
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
            should_quit: false,
            status: None,
            active_tool: None,
            throbber_state: ThrobberState::default(),
        }
    }

    /// advance the spinner state (call each frame)
    pub fn tick(&mut self) {
        self.throbber_state.calc_next();
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

    /// mark a tool as being executed
    pub fn start_tool(&mut self, name: &str, summary: &str) {
        self.active_tool = Some(name.to_string());
        // add to the last message's tool calls if we have one in progress
        if let Some(last) = self.messages.last_mut() {
            last.tool_calls.push(DisplayToolCall {
                name: name.to_string(),
                summary: summary.to_string(),
                status: ToolCallStatus::Running,
                output_preview: None,
            });
        }
    }

    /// mark the current tool as done, with optional output preview
    pub fn end_tool(&mut self, name: &str, is_error: bool, output: Option<&str>) {
        self.active_tool = None;
        if let Some(last) = self.messages.last_mut()
            && let Some(tc) = last.tool_calls.iter_mut().rfind(|t| t.name == name)
        {
            tc.status = if is_error {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Done
            };
            tc.output_preview = output.map(truncate_output);
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
            content: text,
            tool_calls: vec![],
            thinking,
            thinking_expanded: false,
            usage,
            cost,
        });

        if let Some(c) = cost {
            self.total_cost += c;
        }
        if let Some(ref u) = usage {
            self.total_tokens += u.total_tokens();
        }
    }

    /// insert a character at the cursor
    pub fn input_char(&mut self, c: char) {
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

    /// move cursor to start
    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// move cursor to end
    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// clear all messages (for /clear command)
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.scroll_offset = 0;
        self.total_cost = 0.0;
        self.total_tokens = 0;
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
        });
    }

    /// toggle thinking visibility for the last assistant message that has thinking
    pub fn toggle_thinking(&mut self) {
        if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Assistant && m.thinking.is_some())
        {
            msg.thinking_expanded = !msg.thinking_expanded;
        }
    }

    /// take the input text and reset
    pub fn take_input(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }
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
        let app = App::new("test-model".into());
        assert!(app.messages.is_empty());
        assert!(!app.is_streaming);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn push_user_message() {
        let mut app = App::new("test".into());
        app.push_user_message("hello".into());
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::User);
        assert_eq!(app.messages[0].content, "hello");
    }

    #[test]
    fn streaming_lifecycle() {
        let mut app = App::new("test".into());
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
        let mut app = App::new("test".into());
        app.start_streaming();
        app.push_thinking_delta("let me think...");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        assert_eq!(app.messages[0].thinking.as_deref(), Some("let me think..."));
        assert_eq!(app.messages[0].content, "answer");
    }

    #[test]
    fn tool_lifecycle() {
        let mut app = App::new("test".into());
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
        });

        app.start_tool("bash", "ls -la");
        assert_eq!(app.active_tool.as_deref(), Some("bash"));
        assert_eq!(app.messages.last().unwrap().tool_calls.len(), 1);
        assert_eq!(
            app.messages.last().unwrap().tool_calls[0].status,
            ToolCallStatus::Running
        );

        app.end_tool("bash", false, Some("file1.txt\nfile2.txt"));
        assert!(app.active_tool.is_none());
        assert_eq!(
            app.messages.last().unwrap().tool_calls[0].status,
            ToolCallStatus::Done
        );
    }

    #[test]
    fn input_editing() {
        let mut app = App::new("test".into());
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
        let mut app = App::new("test".into());
        app.input = "abc".into();
        app.cursor = 1;
        app.input_delete();
        assert_eq!(app.input, "ac");
    }

    #[test]
    fn take_input_resets() {
        let mut app = App::new("test".into());
        app.input = "hello".into();
        app.cursor = 3;
        let text = app.take_input();
        assert_eq!(text, "hello");
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn cost_accumulates() {
        let mut app = App::new("test".into());
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
        let mut app = App::new("test".into());
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
        });
        app.start_tool("read", "src/main.rs");
        app.end_tool("read", false, Some("fn main() {}\n"));
        let tc = &app.messages.last().unwrap().tool_calls[0];
        assert!(tc.output_preview.is_some());
        assert!(tc.output_preview.as_ref().unwrap().contains("fn main()"));
    }

    #[test]
    fn toggle_thinking() {
        let mut app = App::new("test".into());
        app.start_streaming();
        app.push_thinking_delta("deep thoughts");
        app.push_text_delta("answer");
        app.finish_streaming(None, None);

        // starts collapsed
        assert!(!app.messages[0].thinking_expanded);

        app.toggle_thinking();
        assert!(app.messages[0].thinking_expanded);

        app.toggle_thinking();
        assert!(!app.messages[0].thinking_expanded);
    }

    #[test]
    fn toggle_thinking_targets_last_assistant() {
        let mut app = App::new("test".into());

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

        app.toggle_thinking();
        // should toggle the latest one
        assert!(!app.messages[0].thinking_expanded);
        assert!(app.messages[1].thinking_expanded);
    }

    #[test]
    fn multi_line_input() {
        let mut app = App::new("test".into());
        app.input_char('a');
        app.input_char('\n');
        app.input_char('b');
        assert_eq!(app.input, "a\nb");
        assert_eq!(app.cursor, 3);
    }
}
