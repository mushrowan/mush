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

pub(crate) fn header_text(message: &crate::app::DisplayMessage) -> Option<String> {
    use crate::injected::HeaderPart;
    message.injected.as_ref().map(|row| {
        row.header
            .iter()
            .map(|part| match part {
                HeaderPart::Text(text) | HeaderPart::Who(text) | HeaderPart::Alert(text) => {
                    text.as_str()
                }
            })
            .collect()
    })
}

pub(crate) fn is_compaction_row(message: &crate::app::DisplayMessage) -> bool {
    header_text(message).as_deref() == Some("context compacted")
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

pub(crate) fn tempdir() -> std::io::Result<tempfile::TempDir> {
    if cfg!(target_os = "macos") {
        tempfile::tempdir_in("/private/tmp")
    } else {
        tempfile::tempdir()
    }
}

pub(crate) fn session_meta(
    id: &str,
    title: &str,
    model_id: &str,
    message_count: usize,
    cwd: &str,
) -> mush_session::SessionMeta {
    mush_session::SessionMeta {
        paused: None,
        rlm_max_depth: None,
        spawn_view: None,
        id: id.into(),
        title: Some(title.into()),
        model_id: model_id.into(),
        created_at: mush_ai::types::Timestamp::now(),
        updated_at: mush_ai::types::Timestamp::now(),
        message_count,
        cwd: cwd.into(),
        parent: None,
        subagent_model: None,
        accounts: Default::default(),
        lifecycle: mush_session::Lifecycle::Live,
    }
}
