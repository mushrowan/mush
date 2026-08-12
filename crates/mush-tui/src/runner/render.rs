use std::io::{self, Write};

use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;

use crate::app::{self, App};
use crate::inline::Inline;
use crate::session::Session;
use crate::ui::Ui;
use crate::widgets;

use super::size_cached_backend::SizeCachedBackend;

#[tracing::instrument(name = "draw_panes", skip_all)]
pub(super) fn draw_panes(
    terminal: &mut Terminal<SizeCachedBackend<CrosstermBackend<io::Stdout>>>,
    inline: &mut Inline,
    session: &mut Session,
    image_picker: &Option<ratatui_image::picker::Picker>,
    settings: &crate::settings::ScopedSettings,
) -> io::Result<()> {
    terminal.backend_mut().flush()?;

    session
        .app
        .render_state
        .status_bar_cache
        .borrow_mut()
        .take();
    session.app.pane_info = None;
    session.app.background_alert = None;

    inline.follow_screen(terminal.backend().size()?);
    let mut stdout = io::stdout();
    if let Some(area) = inline.settle(&session.app, &mut stdout)? {
        stdout.flush()?;
        terminal.resize(area)?;
    }

    terminal.draw(|frame| {
        let area = frame.area();
        let (cx, cy) = Ui::new(&session.app).cursor_position(area);

        frame.render_widget(Ui::new(&session.app).inline(true), area);

        let render_areas = session.app.render_state.image_render_areas.borrow().clone();
        if let Some(picker) = image_picker.as_ref() {
            session.ensure_image_protocols(picker, &render_areas);
        }
        for img_area in &render_areas {
            if let Some(proto) = session
                .image_protos
                .get_mut(&(img_area.msg_idx, img_area.tc_idx))
            {
                let widget =
                    ratatui_image::StatefulImage::new().resize(ratatui_image::Resize::Fit(None));
                frame.render_stateful_widget(widget, img_area.area, proto);
            }
        }

        let focused_app = &session.app;
        let streaming_idle = focused_app.is_busy() && focused_app.input.text.is_empty();
        if !streaming_idle
            && (focused_app.interaction.mode == app::AppMode::Normal
                || focused_app.interaction.mode == app::AppMode::SlashComplete
                || (focused_app.stream.active
                    && focused_app.interaction.mode != app::AppMode::ToolConfirm))
        {
            frame.set_cursor_position((cx, cy));
        }

        // centred overlays paint over the whole focused area, the two
        // input-anchored ones need the same layout the pane just used
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

/// the input box's rect inside a pane, recomputed the same way the pane
/// itself laid out. overlays anchored to the input (slash menu, at
/// picker) need it, and getting it wrong floats them over the wrong row
fn input_region(app: &App, pane_area: Rect) -> Rect {
    let input_h = crate::ui::input_height(&app.input, pane_area.width);
    let tools_h =
        crate::widgets::tool_panels::tool_panels_height(&app.active_tools, pane_area.width);
    let status_h = crate::widgets::status_bar::status_bar_height(app, pane_area.width);
    crate::ui::layout(pane_area, input_h, tools_h, status_h).input
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;

    #[test]
    fn message_at_screen_row_finds_correct_message() {
        use crate::app::MessageRowRange;
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(crate::app::DisplayMessage::new(
            crate::app::MessageRole::User,
            "hello",
        ));
        app.messages.push(crate::app::DisplayMessage::new(
            crate::app::MessageRole::Assistant,
            "world",
        ));

        *app.render_state.message_row_ranges.borrow_mut() = vec![
            MessageRowRange {
                msg_idx: 0,
                start: 0,
                end: 3,
            },
            MessageRowRange {
                msg_idx: 1,
                start: 3,
                end: 6,
            },
        ];
        app.render_state.render_scroll.set(0);
        app.render_state.message_area.set(Rect::new(0, 0, 80, 20));

        assert_eq!(app.message_at_screen_row(0), Some(0));
        assert_eq!(app.message_at_screen_row(2), Some(0));
        assert_eq!(app.message_at_screen_row(3), Some(1));
        assert_eq!(app.message_at_screen_row(5), Some(1));
        assert_eq!(app.message_at_screen_row(6), None);
    }

    #[test]
    fn message_at_screen_row_with_render_scroll() {
        use crate::app::MessageRowRange;
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(crate::app::DisplayMessage::new(
            crate::app::MessageRole::User,
            "hello",
        ));
        app.messages.push(crate::app::DisplayMessage::new(
            crate::app::MessageRole::Assistant,
            "world",
        ));

        *app.render_state.message_row_ranges.borrow_mut() = vec![
            MessageRowRange {
                msg_idx: 0,
                start: 0,
                end: 5,
            },
            MessageRowRange {
                msg_idx: 1,
                start: 5,
                end: 10,
            },
        ];
        // scrolled down by 3 lines
        app.render_state.render_scroll.set(3);
        app.render_state.message_area.set(Rect::new(0, 0, 80, 7));

        // screen row 0 -> render line 3 -> msg 0
        assert_eq!(app.message_at_screen_row(0), Some(0));
        // screen row 2 -> render line 5 -> msg 1
        assert_eq!(app.message_at_screen_row(2), Some(1));
    }
}
