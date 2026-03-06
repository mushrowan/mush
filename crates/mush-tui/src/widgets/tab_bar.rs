//! tab bar widget for multi-pane tab mode
//!
//! renders a row of pane labels like "[1] [2*] [3]" where * marks focus

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::pane::PaneManager;

/// renders the tab bar for pane switching
pub struct TabBar<'a> {
    pane_mgr: &'a PaneManager,
}

impl<'a> TabBar<'a> {
    pub fn new(pane_mgr: &'a PaneManager) -> Self {
        Self { pane_mgr }
    }
}

impl Widget for TabBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let focused_idx = self.pane_mgr.focused_index();
        let mut spans = Vec::new();

        for (i, pane) in self.pane_mgr.panes().iter().enumerate() {
            let is_focused = i == focused_idx;
            let num = i + 1;
            let fallback = pane.id.as_u32().to_string();
            let label = pane.label.as_deref().unwrap_or(&fallback);

            let text = if is_focused {
                format!(" {num}:{label}* ")
            } else if pane.app.is_busy() {
                format!(" {num}:{label}… ")
            } else {
                format!(" {num}:{label} ")
            };

            let style = if is_focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if pane.app.has_unread {
                Style::default().fg(Color::Yellow)
            } else if pane.app.is_busy() {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            spans.push(Span::styled(text, style));
        }

        let line = Line::from(spans);
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::pane::{Pane, PaneId};

    fn test_pane(id: u32) -> Pane {
        Pane::new(PaneId::new(id), App::new("test".into(), 200_000))
    }

    #[test]
    fn renders_focused_marker() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        TabBar::new(&mgr).render(area, &mut buf);
        let content: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(content.contains("1:1*"));
        assert!(content.contains("2:2"));
        assert!(!content.contains("2:2*"));
    }

    #[test]
    fn shows_busy_indicator() {
        let mut mgr = PaneManager::new(test_pane(1));
        let mut p2 = test_pane(2);
        p2.app.is_streaming = true;
        mgr.add_pane(p2);
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        TabBar::new(&mgr).render(area, &mut buf);
        let content: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(content.contains('…'));
    }
}
