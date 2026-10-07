use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

use crate::theme::Theme;

const MENU_MAX_ROWS: usize = 12;

#[derive(Debug, Clone, Copy)]
pub struct PopupSize {
    pub width_pct: u16,
    pub height_pct: u16,
    pub width_min: u16,
    pub width_max: u16,
    pub height_min: u16,
    pub height_max: u16,
}

#[must_use]
pub fn centred_popup(area: Rect, size: PopupSize) -> Rect {
    let width = (area.width * size.width_pct / 100)
        .clamp(size.width_min, size.width_max)
        .min(area.width);
    let height = (area.height * size.height_pct / 100)
        .clamp(size.height_min, size.height_max)
        .min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

pub(crate) const PICKER_SIZE: PopupSize = PopupSize {
    width_pct: 80,
    height_pct: 60,
    width_min: 40,
    width_max: 100,
    height_min: 10,
    height_max: 30,
};

pub(crate) fn render_popup_frame<'a>(
    frame: &mut Frame,
    size: PopupSize,
    title: impl Into<Line<'a>>,
    theme: &Theme,
) -> Rect {
    let popup = centred_popup(frame.area(), size);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(theme.search_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    inner
}

pub(crate) fn filter_line<'a>(filter: &'a str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled("filter: ", theme.dim),
        Span::styled(
            if filter.is_empty() { "…" } else { filter },
            Style::default(),
        ),
    ])
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1).max(1)).collect();
    format!("{kept}…")
}

pub(crate) fn render_menu_frame(
    frame: &mut Frame,
    rows: usize,
    input_area: Rect,
    theme: &Theme,
) -> Rect {
    let height = u16::try_from(MENU_MAX_ROWS.min(rows) + 2).unwrap_or(u16::MAX);
    let popup = Rect::new(
        input_area.x,
        input_area.y.saturating_sub(height),
        input_area.width.min(80),
        height,
    );
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.input_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(min_w: u16, max_w: u16, min_h: u16, max_h: u16) -> PopupSize {
        PopupSize {
            width_pct: 80,
            height_pct: 60,
            width_min: min_w,
            width_max: max_w,
            height_min: min_h,
            height_max: max_h,
        }
    }

    #[test]
    fn popup_fits_inside_area_when_min_exceeds_area() {
        let area = Rect::new(0, 0, 39, 26);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        assert!(popup.right() <= area.right(), "popup overflowed right edge");
        assert!(
            popup.bottom() <= area.bottom(),
            "popup overflowed bottom edge"
        );
        assert_eq!(popup.width, 39, "popup width should be capped to area");
    }

    #[test]
    fn popup_height_capped_to_area_when_min_exceeds_area() {
        let area = Rect::new(0, 0, 80, 8);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        assert!(popup.bottom() <= area.bottom());
        assert_eq!(popup.height, 8);
    }

    #[test]
    fn popup_centred_when_smaller_than_area() {
        let area = Rect::new(0, 0, 100, 50);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        assert_eq!(popup.width, 80);
        assert_eq!(popup.x, 10);
        assert_eq!(popup.height, 30);
        assert_eq!(popup.y, 10);
    }

    #[test]
    fn popup_respects_non_zero_area_origin() {
        let area = Rect::new(5, 3, 100, 50);
        let popup = centred_popup(area, size(40, 100, 10, 30));
        assert_eq!(popup.x, 5 + 10);
        assert_eq!(popup.y, 3 + 10);
    }
}
