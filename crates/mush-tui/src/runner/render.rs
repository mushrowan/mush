use std::io;

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use ratatui::{Frame, Terminal};

use crate::app::{App, AppMode};
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
    super::terminal::set_cursor_shape(
        session
            .app
            .interaction
            .mode
            .caret(session.app.interaction.focus),
    );

    terminal.draw(|frame| {
        draw_frame(frame, screen, &mut session.app, settings);
    })?;
    Ok(())
}

fn draw_frame(
    frame: &mut Frame,
    screen: &mut Screen,
    app: &mut App,
    settings: &crate::settings::ScopedSettings,
) {
    app.render_state.status_bar_cache.borrow_mut().take();
    app.pane_info = None;
    app.background_alert = None;

    let area = frame.area();
    let input_cursor = Ui::new(app).cursor_position(area);

    screen.render(app, area, frame.buffer_mut());

    if let Some(ref picker) = app.interaction.session_picker {
        widgets::session_picker::render(frame, picker, &app.theme);
    }
    if let Some(ref picker) = app.interaction.model_picker {
        widgets::model_picker::render(frame, picker, &app.theme);
    }
    if let Some(ref picker) = app.interaction.login_picker {
        widgets::login_picker::render(frame, picker, &app.theme);
    }
    if let Some(ref menu) = app.settings_menu {
        widgets::settings_menu::render(frame, menu, settings, &app.theme);
    }
    if let Some(ref recovery) = app.interaction.compaction_recovery {
        widgets::compaction_recovery::render(frame, recovery, &app.theme);
    }

    let needs_input_anchor =
        app.completion.slash_menu.is_some() || app.interaction.at_picker.is_some();
    if needs_input_anchor {
        let input_area = input_region(app, area);
        if let Some(ref menu) = app.completion.slash_menu {
            widgets::slash_menu::render(frame, menu, input_area, &app.theme);
        }
        if let Some(ref picker) = app.interaction.at_picker {
            widgets::at_picker::render(frame, picker, input_area, &app.theme);
        }
    }

    if app.subagents.open {
        crate::subagents::render::render(frame, &app.subagents, &app.theme, chrono::Utc::now());
        return;
    }

    let busy_empty_input = app.is_busy() && app.input.text.is_empty();
    let cursor = match app.interaction.mode {
        AppMode::Normal | AppMode::Visual if app.interaction.focus.is_log() => {
            screen.cursor_cell(app, area)
        }
        AppMode::Insert | AppMode::SlashComplete | AppMode::Normal | AppMode::Visual => {
            (!busy_empty_input).then_some(input_cursor)
        }
        AppMode::SessionPicker
        | AppMode::ModelPicker
        | AppMode::LoginPicker
        | AppMode::AtPicker
        | AppMode::SlotEdit
        | AppMode::ToolConfirm
        | AppMode::Search
        | AppMode::Settings
        | AppMode::CompactionRecovery => None,
    };
    if let Some(cell) = cursor {
        frame.set_cursor_position(cell);
    }
}

fn input_region(app: &App, pane_area: Rect) -> Rect {
    let input_h = crate::ui::input_height(&app.input, pane_area.width);
    let status_h = crate::widgets::status_bar::status_bar_height(app, pane_area.width);
    crate::ui::layout(pane_area, input_h, status_h).input
}

#[cfg(test)]
mod tests;
