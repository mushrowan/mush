use std::io;

use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::app::{self, App};
use crate::pane::{LayoutMode, PaneManager};
use crate::ui::Ui;
use crate::widgets;

pub(super) fn draw_panes(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    pane_mgr: &mut PaneManager,
    _image_picker: &Option<ratatui_image::picker::Picker>,
) -> io::Result<()> {
    let pane_count = pane_mgr.pane_count() as u16;
    let focused_idx = pane_mgr.focused_index();
    if pane_count > 1 {
        let alert: Option<String> = {
            let busy: Vec<String> = pane_mgr
                .panes()
                .iter()
                .enumerate()
                .filter(|(i, pane)| *i != focused_idx && pane.app.is_busy())
                .map(|(i, _)| format!("pane {}", i + 1))
                .collect();
            if busy.is_empty() {
                None
            } else {
                Some(format!("{}: busy", busy.join(", ")))
            }
        };
        for (i, pane) in pane_mgr.panes_mut().iter_mut().enumerate() {
            pane.app.pane_info = Some(((i + 1) as u16, pane_count));
            pane.app.background_alert = if i == focused_idx {
                alert.clone()
            } else {
                None
            };
        }
    } else {
        pane_mgr.panes_mut()[0].app.pane_info = None;
        pane_mgr.panes_mut()[0].app.background_alert = None;
    }

    terminal.draw(|frame| {
        let area = frame.area();
        let focused_idx = pane_mgr.focused_index();
        let pane_count = pane_mgr.pane_count();
        let is_multi = pane_count > 1;

        let shared_status_h = if is_multi {
            crate::widgets::status_bar::status_bar_height(
                &pane_mgr.panes()[focused_idx].app,
                area.width,
            )
        } else {
            0
        };
        let panes_area = Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(shared_status_h),
        );

        let mode = pane_mgr.compute_layout(panes_area);

        if mode == LayoutMode::Tabs && pane_count > 1 {
            let tab_area =
                ratatui::layout::Rect::new(panes_area.x, panes_area.y, panes_area.width, 1);
            frame.render_widget(crate::widgets::tab_bar::TabBar::new(&*pane_mgr), tab_area);
        }

        let focused_area = pane_mgr.panes()[focused_idx].area;
        let (cx, cy) = Ui::new(&pane_mgr.panes()[focused_idx].app)
            .hide_status(is_multi)
            .cursor_position(focused_area);

        if mode == LayoutMode::Columns && pane_count > 1 {
            let buf = frame.buffer_mut();
            for (i, pane) in pane_mgr.panes().iter().enumerate() {
                if i == 0 {
                    continue;
                }
                let sep_x = pane.area.x.saturating_sub(1);
                let is_adjacent_to_focus = i == focused_idx || i == focused_idx + 1;
                let style = if is_adjacent_to_focus {
                    pane_mgr.panes()[focused_idx]
                        .app
                        .theme
                        .pane_separator_active
                } else {
                    pane_mgr.panes()[focused_idx].app.theme.pane_separator
                };
                for y in panes_area.y..panes_area.y + panes_area.height {
                    if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(sep_x, y)) {
                        cell.set_symbol("│").set_style(style);
                    }
                }
            }
        }

        for (i, pane) in pane_mgr.panes_mut().iter_mut().enumerate() {
            if mode == LayoutMode::Tabs && i != focused_idx {
                continue;
            }
            let pane_area = pane.area;
            frame.render_widget(Ui::new(&pane.app).hide_status(is_multi), pane_area);

            let render_areas = pane.app.image_render_areas.borrow().clone();
            for img_area in &render_areas {
                if let Some(proto) = pane
                    .image_protos
                    .get_mut(&(img_area.msg_idx, img_area.tc_idx))
                {
                    let widget = ratatui_image::StatefulImage::new()
                        .resize(ratatui_image::Resize::Fit(None));
                    frame.render_stateful_widget(widget, img_area.area, proto);
                }
            }
        }

        if is_multi && shared_status_h > 0 {
            let status_area = Rect::new(
                area.x,
                panes_area.y + panes_area.height,
                area.width,
                shared_status_h,
            );
            frame.render_widget(
                crate::widgets::status_bar::StatusBar::new(&pane_mgr.panes()[focused_idx].app),
                status_area,
            );
        }

        let focused_app = &pane_mgr.panes()[focused_idx].app;
        let streaming_idle = focused_app.is_busy() && focused_app.input.text.is_empty();
        if !streaming_idle
            && (focused_app.mode == app::AppMode::Normal
                || focused_app.mode == app::AppMode::SlashComplete
                || (focused_app.stream.active && focused_app.mode != app::AppMode::ToolConfirm))
        {
            frame.set_cursor_position((cx, cy));
        }

        if let Some(ref picker) = focused_app.session_picker {
            widgets::session_picker::render(frame, picker, &focused_app.theme);
        }
        if let Some(ref menu) = focused_app.slash_menu {
            let input_h = crate::ui::input_height(&focused_app.input, focused_area.width);
            let tools_h = crate::widgets::tool_panels::tool_panels_height(
                &focused_app.active_tools,
                focused_area.width,
            );
            let status_h = if is_multi {
                0
            } else {
                crate::widgets::status_bar::status_bar_height(focused_app, focused_area.width)
            };
            let regions = crate::ui::layout(focused_area, input_h, tools_h, status_h);
            widgets::slash_menu::render(frame, menu, regions.input, &focused_app.theme);
        }
    })?;
    Ok(())
}

pub(super) fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    const SCROLL_LINES: u16 = 3;

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.input.is_mouse_over(mouse.column, mouse.row) {
                app.input.scroll_by(-(SCROLL_LINES as i16));
            } else {
                app.scroll_offset = app.scroll_offset.saturating_add(SCROLL_LINES);
            }
        }
        MouseEventKind::ScrollDown => {
            if app.input.is_mouse_over(mouse.column, mouse.row) {
                app.input.scroll_by(SCROLL_LINES as i16);
            } else {
                app.scroll_offset = app.scroll_offset.saturating_sub(SCROLL_LINES);
                if app.scroll_offset == 0 {
                    app.has_unread = false;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;

    #[test]
    fn mouse_scroll_over_messages_scrolls_conversation() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.area.set(ratatui::layout::Rect::new(0, 10, 40, 5));
        let before = app.scroll_offset;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert!(app.scroll_offset > before);
    }

    #[test]
    fn mouse_scroll_over_input_scrolls_input() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.area.set(ratatui::layout::Rect::new(0, 10, 40, 5));
        app.input.visible_lines.set(2);
        app.input.total_lines.set(8);
        app.input.scroll.set(2);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 11,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.input.scroll.get(), 0);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 1,
                row: 11,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.input.scroll.get(), 3);
    }
}
