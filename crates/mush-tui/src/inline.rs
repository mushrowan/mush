//! the inline viewport: settled history the terminal owns, a live block
//! mush redraws

use std::io::{self, Write};

use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use ratatui::layout::{Rect, Size};

use crate::app::App;
use crate::live_region::LiveRegion;
use crate::scrollback::{Scrollback, write_lines};
use crate::ui::live_height;
use crate::widgets::message_list::MessageStyle;

/// drives one frame of the inline ui: what settled goes to the terminal,
/// and the live block follows it down the screen
#[derive(Debug)]
pub struct Inline {
    scrollback: Scrollback,
    region: LiveRegion,
}

impl Inline {
    #[must_use]
    pub fn new(screen: Size) -> Self {
        Self {
            scrollback: Scrollback::new(),
            region: LiveRegion::new(screen, 0),
        }
    }

    /// print what settled since the last frame, and say where the live
    /// block sits now
    pub fn settle(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        out: &mut impl Write,
    ) -> io::Result<Rect> {
        let area = self.region.area();
        let lines = self.scrollback.take_settled(app, style, area.width);
        if !lines.is_empty() {
            out.queue(MoveTo(area.x, area.y))?;
            write_lines(out, &lines)?;
            self.region
                .emit(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        }
        self.region.set_height(live_height(app, area.width));
        Ok(self.region.area())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use mush_ai::types::TokenCount;

    fn app_mid_turn() -> App {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_user_message("settled question");
        app.start_streaming();
        app.push_text_delta("settled answer");
        app.finish_streaming(None, None);
        app.push_user_message("live question");
        app.start_streaming();
        app.push_text_delta("live answer");
        for _ in 0..200 {
            app.tick();
        }
        app
    }

    fn settle(inline: &mut Inline, app: &App) -> (String, Rect) {
        let theme = Theme::default();
        let style = MessageStyle {
            theme: &theme,
            show_usage: false,
        };
        let mut out = Vec::new();
        let area = inline.settle(app, &style, &mut out).unwrap();
        (String::from_utf8(out).unwrap(), area)
    }

    #[test]
    fn a_settled_turn_goes_out_above_the_live_block() {
        let mut inline = Inline::new(Size::new(40, 40));
        let (written, area) = settle(&mut inline, &app_mid_turn());

        assert!(written.contains("settled answer"), "got:\n{written}");
        assert!(!written.contains("live answer"), "still live:\n{written}");
        assert_eq!(usize::from(area.y), written.matches("\r\n").count());
    }

    #[test]
    fn the_live_block_is_as_tall_as_the_turn_in_it() {
        let mut inline = Inline::new(Size::new(40, 40));
        let (_, idle) = settle(
            &mut inline,
            &App::new("test".into(), TokenCount::new(200_000)),
        );

        let mut inline = Inline::new(Size::new(40, 40));
        let (_, mid_turn) = settle(&mut inline, &app_mid_turn());

        assert!(mid_turn.height > idle.height, "{mid_turn:?} vs {idle:?}");
    }
}
