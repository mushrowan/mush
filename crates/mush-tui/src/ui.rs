use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::app::App;
use crate::widgets::input_box::InputBox;
pub use crate::widgets::status_bar::status_bar_height;
use crate::widgets::top_bar::TOP_BAR_HEIGHT;

pub struct Ui<'a> {
    app: &'a App,
}

impl<'a> Ui<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let input_h = input_height(&self.app.input, area.width);
        let status_h = status_bar_height(self.app, area.width);
        let chunks = layout(area, input_h, status_h);
        self.app.input.area.set(chunks.input);
        self.app
            .input
            .visible_lines
            .set(chunks.input.height.saturating_sub(2));
        self.app.input.ensure_cursor_visible();
        InputBox::new(self.app).cursor_position(chunks.input)
    }
}

pub struct LayoutRegions {
    pub top: Rect,
    pub messages: Rect,
    pub input: Rect,
    pub status: Rect,
}

pub fn input_height(input: &crate::app::InputBuffer, area_width: u16) -> u16 {
    let content_width = area_width.saturating_sub(2) as usize;
    if content_width == 0 {
        return 3;
    }

    let layout = input.layout(content_width);
    (layout.total_lines + 2).min(12)
}

pub fn layout(area: Rect, input_h: u16, status_h: u16) -> LayoutRegions {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(TOP_BAR_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(input_h),
            Constraint::Length(status_h),
        ])
        .split(area);

    LayoutRegions {
        top: chunks[0],
        messages: chunks[1],
        input: chunks[2],
        status: chunks[3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::buffer_to_string;
    use mush_ai::types::{Dollars, TokenCount};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    #[test]
    fn input_height_short_text() {
        assert_eq!(input_height_for("hi", 80, vec![]), 3);
    }

    #[test]
    fn input_height_wrapping() {
        let text = "a".repeat(100);
        assert_eq!(input_height_for(&text, 20, vec![]), 8);
    }

    #[test]
    fn input_height_capped() {
        let text = "a".repeat(1000);
        assert_eq!(input_height_for(&text, 20, vec![]), 12);
    }

    #[test]
    fn input_height_multiline() {
        assert_eq!(input_height_for("hello\nworld", 40, vec![]), 4);
        assert_eq!(input_height_for("a\nb\nc\nd", 40, vec![]), 6);
    }

    #[test]
    fn input_height_with_images() {
        use crate::app::{IMAGE_PLACEHOLDER, PendingImage};
        use mush_ai::types::ImageMimeType;
        let img = PendingImage {
            data: vec![],
            mime_type: ImageMimeType::Png,
            dimensions: Some((100, 200)),
        };
        let input = format!("hi{IMAGE_PLACEHOLDER}");
        assert_eq!(input_height_for(&input, 80, vec![img]), 3);
    }

    #[test]
    fn ui_sets_input_area_for_mouse_routing() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let ui = Ui::new(&app);
        let area = Rect::new(0, 0, 80, 24);
        let _ = ui.cursor_position(area);
        let input = app.input.area.get();
        assert!(input.height > 0);
        assert!(input.y > 0);
    }

    fn render_full(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut screen = crate::screen::Screen::new();
        terminal
            .draw(|frame| screen.render(app, frame.area(), frame.buffer_mut()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn full_layout_renders() {
        let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
        app.push_user_message("what is rust?");
        app.start_streaming();
        app.push_text_delta("rust is a systems programming language");
        app.finish_streaming(None, Some(Dollars::new(0.003)));

        let buf = render_full(&app, 60, 20);
        let content = buffer_to_string(&buf);

        assert!(content.contains("what is rust"));
        assert!(content.contains("systems programming"));
        assert!(content.contains("> "));
    }

    #[test]
    fn layout_regions_are_correct() {
        let area = Rect::new(0, 0, 80, 24);
        let regions = layout(area, 3, 1);
        assert_eq!(regions.top.height, 1);
        assert_eq!(regions.top.y, 0);
        assert_eq!(regions.status.height, 1);
        assert_eq!(regions.input.height, 3);
        assert_eq!(regions.messages.height, 19);
        assert_eq!(regions.messages.y, 1);
    }

    #[test]
    fn cursor_position_in_layout() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = 5;
        let ui = Ui::new(&app);
        let area = Rect::new(0, 0, 200, 24);
        let (x, y) = ui.cursor_position(area);
        assert_eq!(y, 21);
        assert_eq!(x, 8);
    }

    #[test]
    fn input_layout_cache_reuses_layout_across_cursor_and_render() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "one two three four five six seven eight nine ten".into();
        app.input.cursor = app.input.text.len();
        app.input.reset_layout_builds();

        let area = Rect::new(0, 0, 20, 8);
        let _ = Ui::new(&app).cursor_position(area);
        let _buf = render_full(&app, 20, 8);

        assert_eq!(app.input.layout_builds(), 1);
    }

    #[test]
    fn input_layout_cache_refreshes_after_text_change() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = app.input.text.len();
        app.input.reset_layout_builds();

        assert_eq!(input_height(&app.input, 20), 3);
        assert_eq!(app.input.layout_builds(), 1);

        app.input.text.push('!');
        app.input.cursor = app.input.text.len();

        assert_eq!(input_height(&app.input, 20), 3);
        assert_eq!(app.input.layout_builds(), 2);
    }

    fn input_height_for(text: &str, width: u16, images: Vec<crate::app::PendingImage>) -> u16 {
        let mut input = crate::app::InputBuffer::new();
        input.text = text.to_string();
        input.cursor = input.text.len();
        input.images = images;
        input_height(&input, width)
    }

    #[test]
    fn cursor_position_correct_after_newline_grows_input() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = 5;

        let area = Rect::new(0, 0, 40, 24);
        let _ = Ui::new(&app).cursor_position(area);
        let _ = render_full(&app, 40, 24);

        app.input.insert_char('\n');

        let (_, cy) = Ui::new(&app).cursor_position(area);

        let input_area = app.input.area.get();
        let expected_y = input_area.y + 1 + 1;
        assert_eq!(
            cy, expected_y,
            "cursor should be on the new line, not the previous one"
        );
    }
}
