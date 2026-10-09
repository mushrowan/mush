use std::io;

use ratatui::backend::{Backend, CrosstermBackend};
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
    if session.app.render_state.force_clear.replace(false) {
        terminal.clear()?;
    }
    let title = terminal_title(&session.app);
    if screen.title.as_deref() != Some(title.as_str()) {
        super::terminal::set_title(&title);
        screen.title = Some(title);
    }
    super::terminal::set_cursor_shape(
        session
            .app
            .interaction
            .mode
            .caret(session.app.interaction.focus),
    );

    let drawn = super::terminal::catch_render_panic(|| {
        terminal.draw(|frame| {
            draw_frame(frame, screen, &mut session.app, settings);
        })
    });
    match drawn {
        Ok(result) => result?,
        Err(report) => {
            tracing::error!(%report, "render panicked");
            if screen.render_panic.as_deref() != Some(report.as_str()) {
                session
                    .app
                    .push_system_message(format!("a render panicked: {report}"));
                screen.render_panic = Some(report);
            }
            return Ok(());
        }
    };
    Ok(())
}

pub(crate) fn terminal_title(app: &App) -> String {
    let cwd = app
        .cwd
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(&app.cwd);
    match app
        .session_title
        .as_deref()
        .map(widgets::top_bar::sanitise_title)
        .filter(|title| !title.is_empty())
    {
        Some(title) => format!("mush - {title} - {cwd}"),
        None => format!("mush - {cwd}"),
    }
}

fn draw_frame(
    frame: &mut Frame,
    screen: &mut Screen,
    app: &mut App,
    settings: &crate::settings::ScopedSettings,
) {
    app.flush_status();
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
    if let Some(ref picker) = app.interaction.message_picker {
        widgets::message_picker::render(frame, picker, &app.theme);
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
    widgets::question_dialog::render(app, area, frame.buffer_mut());

    let needs_input_anchor =
        app.completion.slash_menu.is_some() || app.interaction.at_picker.is_some();
    if needs_input_anchor {
        let input_area = crate::ui::app_layout(app, area).input;
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
        | AppMode::MessagePicker
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

#[cfg(test)]
mod tests;
