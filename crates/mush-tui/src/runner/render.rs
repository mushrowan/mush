use std::io;

use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;

use crate::app::{self, App};
use crate::screen::Screen;
use crate::session::Session;
use crate::ui::Ui;
use crate::widgets;

use super::size_cached_backend::SizeCachedBackend;

#[tracing::instrument(name = "draw_panes", skip_all)]
pub(super) fn draw_panes(
    terminal: &mut Terminal<SizeCachedBackend<CrosstermBackend<io::Stdout>>>,
    screen: &mut Screen,
    session: &mut Session,
    settings: &crate::settings::ScopedSettings,
) -> io::Result<()> {
    terminal.backend_mut().flush()?;
    super::terminal::set_cursor_shape(session.app.interaction.mode.caret());

    session
        .app
        .render_state
        .status_bar_cache
        .borrow_mut()
        .take();
    session.app.pane_info = None;
    session.app.background_alert = None;

    terminal.draw(|frame| {
        let area = frame.area();
        let (cx, cy) = Ui::new(&session.app).cursor_position(area);

        screen.render(&session.app, area, frame.buffer_mut());

        let focused_app = &session.app;
        if let Some(cell) = screen.cursor_cell(focused_app, area) {
            frame.set_cursor_position(cell);
        }
        let streaming_idle = focused_app.is_busy() && focused_app.input.text.is_empty();
        if !streaming_idle
            && (focused_app.interaction.mode == app::AppMode::Normal
                || focused_app.interaction.mode == app::AppMode::SlashComplete
                || (focused_app.stream.active
                    && focused_app.interaction.mode != app::AppMode::ToolConfirm))
        {
            frame.set_cursor_position((cx, cy));
        }

        if let Some(ref picker) = focused_app.interaction.session_picker {
            widgets::session_picker::render(frame, picker, &focused_app.theme);
        }
        if let Some(ref picker) = focused_app.interaction.model_picker {
            widgets::model_picker::render(frame, picker, &focused_app.theme);
        }
        if let Some(ref picker) = focused_app.interaction.login_picker {
            widgets::login_picker::render(frame, picker, &focused_app.theme);
        }
        if let Some(ref menu) = focused_app.settings_menu {
            widgets::settings_menu::render(frame, menu, settings, &focused_app.theme);
        }
        if let Some(ref recovery) = focused_app.interaction.compaction_recovery {
            widgets::compaction_recovery::render(frame, recovery, &focused_app.theme);
        }

        let needs_input_anchor = focused_app.completion.slash_menu.is_some()
            || focused_app.interaction.at_picker.is_some();
        if needs_input_anchor {
            let input_area = input_region(focused_app, area);
            if let Some(ref menu) = focused_app.completion.slash_menu {
                widgets::slash_menu::render(frame, menu, input_area, &focused_app.theme);
            }
            if let Some(ref picker) = focused_app.interaction.at_picker {
                widgets::at_picker::render(frame, picker, input_area, &focused_app.theme);
            }
        }
    })?;
    Ok(())
}

fn input_region(app: &App, pane_area: Rect) -> Rect {
    let input_h = crate::ui::input_height(&app.input, pane_area.width);
    let status_h = crate::widgets::status_bar::status_bar_height(app, pane_area.width);
    crate::ui::layout(pane_area, input_h, status_h).input
}
