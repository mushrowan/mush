use ratatui::buffer::Buffer;

pub(crate) fn buffer_to_string(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

pub(crate) fn user_message(text: &str) -> mush_ai::types::Message {
    mush_ai::types::Message::User(
        mush_ai::types::UserMessage::builder()
            .content(mush_ai::types::UserContent::Text(text.into()))
            .timestamp_ms(mush_ai::types::Timestamp::zero())
            .build(),
    )
}

pub(crate) fn draw_to_buffer(
    width: u16,
    height: u16,
    draw: impl FnOnce(&mut ratatui::Frame<'_>),
) -> Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal.draw(draw).expect("draw");
    terminal.backend().buffer().clone()
}
