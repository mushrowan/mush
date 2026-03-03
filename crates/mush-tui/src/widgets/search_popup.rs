//! floating search popup for conversation search

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget};

use crate::app::App;

/// renders a floating search popup over the main UI
pub struct SearchPopup<'a> {
    app: &'a App,
}

impl<'a> SearchPopup<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }
}

impl Widget for SearchPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // centre the popup, 60% width, up to 14 lines tall
        let width = (area.width * 3 / 5)
            .max(30)
            .min(area.width.saturating_sub(4));
        let height = 14.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let popup = Rect::new(x, y, width, height);

        // clear background
        Clear.render(popup, buf);

        let block = Block::default()
            .title(" search ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(popup);
        block.render(popup, buf);

        if inner.height < 3 {
            return;
        }

        // split: input line at top, results below
        let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
        let divider_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
        let results_area = Rect::new(
            inner.x,
            inner.y + 2,
            inner.width,
            inner.height.saturating_sub(2),
        );

        // input line
        let query_spans = vec![
            Span::styled("/ ", Style::default().fg(Color::Cyan)),
            Span::raw(&self.app.search.query),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ];
        Paragraph::new(Line::from(query_spans)).render(input_area, buf);

        // divider
        let match_count = self.app.search.matches.len();
        let divider_text = if self.app.search.query.is_empty() {
            "type to search".to_string()
        } else {
            format!(
                "{match_count} match{}",
                if match_count == 1 { "" } else { "es" }
            )
        };
        Paragraph::new(Line::styled(
            divider_text,
            Style::default().fg(Color::DarkGray),
        ))
        .render(divider_area, buf);

        // results list
        let items: Vec<ListItem> = self
            .app
            .search
            .matches
            .iter()
            .enumerate()
            .map(|(i, &msg_idx)| {
                let msg = &self.app.messages[msg_idx];
                let role = match msg.role {
                    crate::app::MessageRole::User => "you",
                    crate::app::MessageRole::Assistant => "assistant",
                    crate::app::MessageRole::System => "system",
                };
                // preview: first line of content, truncated
                let preview = msg
                    .content
                    .lines()
                    .find(|l| !l.is_empty())
                    .unwrap_or("(empty)");
                let max_len = (results_area.width as usize).saturating_sub(role.len() + 4);
                let truncated = if preview.len() > max_len {
                    format!("{}…", &preview[..max_len.saturating_sub(1)])
                } else {
                    preview.to_string()
                };

                let selected = i == self.app.search.selected;
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let marker = if selected { "▸ " } else { "  " };

                ListItem::new(Line::from(vec![
                    Span::styled(marker, style),
                    Span::styled(format!("{role}: "), Style::default().fg(Color::DarkGray)),
                    Span::styled(truncated, style),
                ]))
            })
            .collect();

        List::new(items).render(results_area, buf);
    }
}
