use mush_tui::display_types::{
    DisplayMessage, ImageRenderArea, MessageRole, MessageRowRange, ToolCallStatus,
};
use ratatui::layout::Rect;

#[test]
fn display_types_module_exposes_message_and_render_types() {
    let message = DisplayMessage::new(MessageRole::Assistant, "hello");
    assert_eq!(message.content, "hello");
    assert!(message.tool_calls.is_empty());
    assert_eq!(ToolCallStatus::Done, ToolCallStatus::Done);

    let area = ImageRenderArea {
        msg_idx: 1,
        tc_idx: 2,
        area: Rect::new(3, 4, 5, 6),
    };
    assert_eq!(area.area, Rect::new(3, 4, 5, 6));

    let rows = MessageRowRange {
        msg_idx: 7,
        start: 8,
        end: 9,
    };
    assert_eq!(rows.end, 9);
}
