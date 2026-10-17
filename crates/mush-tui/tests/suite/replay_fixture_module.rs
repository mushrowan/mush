use mush_ai::types::TokenCount;
use mush_session::{SessionId, SessionStore, StoreError};
use mush_tui::app::{App, ChatDetail};
use mush_tui::screen::Screen;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

const FIXTURE: &str = "replay0000000001";

fn replayed() -> Result<App, StoreError> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay");
    let session = SessionStore::new(dir).load(&SessionId::from(FIXTURE))?;
    let messages = session
        .conversation
        .tree()
        .recent_display_history(2000)
        .messages;
    let mut app = App::new(session.meta.model_id.clone(), TokenCount::new(200_000));
    mush_tui::slash::rebuild_display(&mut app, &messages);
    Ok(app)
}

fn screen_text(app: &App, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    Screen::new().render(app, area, &mut buffer);
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_replay_fixture_loads_as_a_conversation_with_tools() -> Result<(), StoreError> {
    let app = replayed()?;

    assert_eq!(app.messages.len(), 18);
    let tools: Vec<&str> = app
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .map(|call| call.name.as_str())
        .collect();
    assert_eq!(tools.len(), 12);
    assert!(tools.iter().all(|name| ["ipython", "bash"].contains(name)));
    assert!(
        app.messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .filter(|call| call.name == "ipython")
            .all(|call| call.cell.is_some())
    );
    Ok(())
}

#[test]
fn the_replay_fixture_draws_at_every_detail_level_and_width() -> Result<(), StoreError> {
    let mut app = replayed()?;

    for detail in [ChatDetail::Overview, ChatDetail::Details, ChatDetail::All] {
        app.detail = detail;
        for (width, height) in [(100, 30), (48, 20)] {
            let text = screen_text(&app, width, height);
            assert!(
                text.contains("round 5 done"),
                "{detail:?} {width}x{height}\n{text}"
            );
        }
    }
    Ok(())
}
