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
    /// block goes when it has to be redrawn from scratch: it moved, it
    /// resized, or the screen scrolled out from under it
    pub fn settle(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        out: &mut impl Write,
    ) -> io::Result<Option<Rect>> {
        let before = self.region.area();
        let lines = self.scrollback.take_settled(app, style, before.width);
        let mut scrolled = 0;
        if !lines.is_empty() {
            out.queue(MoveTo(before.x, before.y))?;
            write_lines(out, &lines)?;
            scrolled += self
                .region
                .emit(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        }
        scrolled += self.region.set_height(live_height(app, before.width));

        let area = self.region.area();
        Ok((scrolled > 0 || area != before).then_some(area))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use mush_ai::types::TokenCount;

    fn app_mid_turn() -> App {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        finish_turn(&mut app, "settled question", "settled answer");
        stream_turn(&mut app, "live question", "live answer");
        app
    }

    fn settle(inline: &mut Inline, app: &App) -> (String, Option<Rect>) {
        let theme = Theme::default();
        let style = MessageStyle {
            theme: &theme,
            show_usage: false,
        };
        let mut out = Vec::new();
        let area = inline.settle(app, &style, &mut out).unwrap();
        (String::from_utf8(out).unwrap(), area)
    }

    fn stream_turn(app: &mut App, prompt: &str, reply: &str) {
        app.push_user_message(prompt);
        app.start_streaming();
        app.push_text_delta(reply);
        for _ in 0..200 {
            app.tick();
        }
    }

    fn finish_turn(app: &mut App, prompt: &str, reply: &str) {
        stream_turn(app, prompt, reply);
        app.finish_streaming(None, None);
    }

    #[test]
    fn a_settled_turn_goes_out_above_the_live_block() {
        let mut inline = Inline::new(Size::new(40, 40));
        let (written, area) = settle(&mut inline, &app_mid_turn());

        assert!(written.contains("settled answer"), "got:\n{written}");
        assert!(!written.contains("live answer"), "still live:\n{written}");
        let area = area.expect("the block moved down past the settled lines");
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

        let (idle, mid_turn) = (idle.unwrap(), mid_turn.unwrap());
        assert!(mid_turn.height > idle.height, "{mid_turn:?} vs {idle:?}");
    }

    #[test]
    fn a_frame_that_moves_nothing_asks_for_no_resize() {
        let mut inline = Inline::new(Size::new(40, 40));
        let app = app_mid_turn();
        settle(&mut inline, &app);

        assert_eq!(settle(&mut inline, &app).1, None);
    }

    #[test]
    fn a_frame_that_scrolls_the_screen_asks_for_a_resize() {
        // pinned against the bottom, so settled lines scroll the screen
        // out from under ratatui's back buffer
        let mut inline = Inline::new(Size::new(40, 12));
        let mut app = app_mid_turn();
        let (_, placed) = settle(&mut inline, &app);

        app.finish_streaming(None, None);
        stream_turn(&mut app, "live question", "live answer");
        let (_, after) = settle(&mut inline, &app);

        assert_eq!(after, placed, "the block cannot move any further down");
        assert!(after.is_some(), "the screen scrolled, so redraw all of it");
    }
}
