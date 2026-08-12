//! the inline viewport: settled history the terminal owns, a live block
//! mush redraws

use std::io::{self, Write};

use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use crossterm::terminal::{Clear, ClearType};
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
    /// `top` is the row the cursor is on, so the block opens below
    /// whatever the shell left on screen
    #[must_use]
    pub fn new(screen: Size, top: u16) -> Self {
        Self {
            scrollback: Scrollback::new(),
            region: LiveRegion::new(screen, top),
        }
    }

    /// where the block sits now
    #[must_use]
    pub fn area(&self) -> Rect {
        self.region.area()
    }

    /// keep the block on a screen this size, placing it again when the
    /// size changed under it
    pub fn follow_screen(&mut self, screen: Size) {
        if screen != self.region.screen() {
            self.region.reset(screen);
        }
    }

    /// hand the rest of the conversation over and clear the block, so
    /// the shell prompt lands after the last message
    pub fn finish(&mut self, app: &App, out: &mut impl Write) -> io::Result<()> {
        let area = self.region.area();
        let lines = self.scrollback.take_rest(app, &style_of(app), area.width);
        out.queue(MoveTo(area.x, area.y))?;
        write_lines(out, &lines)?;
        out.queue(Clear(ClearType::FromCursorDown))?;
        out.flush()
    }

    /// print what settled since the last frame, and say where the live
    /// block goes when it has to be redrawn from scratch: it moved, it
    /// resized, or the screen scrolled out from under it
    pub fn settle(&mut self, app: &App, out: &mut impl Write) -> io::Result<Option<Rect>> {
        let before = self.region.area();
        let lines = self
            .scrollback
            .take_settled(app, &style_of(app), before.width);
        if !lines.is_empty() {
            out.queue(MoveTo(before.x, before.y))?;
            write_lines(out, &lines)?;
        }

        let printed = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let room = self.region.advance(printed, live_height(app, before.width));
        if room > 0 {
            out.queue(MoveTo(0, self.region.bottom_row()))?;
            write!(out, "{}", "\n".repeat(usize::from(room)))?;
        }

        let area = self.region.area();
        Ok((room > 0 || area != before).then_some(area))
    }
}

fn style_of(app: &App) -> MessageStyle<'_> {
    MessageStyle {
        theme: &app.theme,
        show_usage: app.interaction.show_usage_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;

    fn app_mid_turn() -> App {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        finish_turn(&mut app, "settled question", "settled answer");
        stream_turn(&mut app, "live question", "live answer");
        app
    }

    fn settle(inline: &mut Inline, app: &App) -> (String, Option<Rect>) {
        let mut out = Vec::new();
        let area = inline.settle(app, &mut out).unwrap();
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
        let mut inline = Inline::new(Size::new(40, 40), 0);
        let (written, area) = settle(&mut inline, &app_mid_turn());

        assert!(written.contains("settled answer"), "got:\n{written}");
        assert!(!written.contains("live answer"), "still live:\n{written}");
        let area = area.expect("the block moved down past the settled lines");
        assert_eq!(usize::from(area.y), written.matches("\r\n").count());
    }

    #[test]
    fn the_live_block_is_as_tall_as_the_turn_in_it() {
        let mut inline = Inline::new(Size::new(40, 40), 0);
        let (_, idle) = settle(
            &mut inline,
            &App::new("test".into(), TokenCount::new(200_000)),
        );

        let mut inline = Inline::new(Size::new(40, 40), 0);
        let (_, mid_turn) = settle(&mut inline, &app_mid_turn());

        let (idle, mid_turn) = (idle.unwrap(), mid_turn.unwrap());
        assert!(mid_turn.height > idle.height, "{mid_turn:?} vs {idle:?}");
    }

    #[test]
    fn a_frame_that_moves_nothing_asks_for_no_resize() {
        let mut inline = Inline::new(Size::new(40, 40), 0);
        let app = app_mid_turn();
        settle(&mut inline, &app);

        assert_eq!(settle(&mut inline, &app).1, None);
    }

    #[test]
    fn a_frame_that_scrolls_the_screen_asks_for_a_resize() {
        // pinned against the bottom, so settled lines scroll the screen
        // out from under ratatui's back buffer
        let mut inline = Inline::new(Size::new(40, 12), 0);
        let mut app = app_mid_turn();
        let (_, placed) = settle(&mut inline, &app);

        app.finish_streaming(None, None);
        stream_turn(&mut app, "live question", "live answer");
        let (_, after) = settle(&mut inline, &app);

        assert_eq!(after, placed, "the block cannot move any further down");
        assert!(after.is_some(), "the screen scrolled, so redraw all of it");
    }

    #[test]
    fn a_block_with_the_screen_to_itself_asks_for_no_scroll() {
        let mut inline = Inline::new(Size::new(40, 40), 0);
        let (written, _) = settle(&mut inline, &app_mid_turn());

        assert_eq!(scrolls(&written), 0, "got:\n{written:?}");
    }

    #[test]
    fn a_block_against_the_bottom_scrolls_the_screen_to_fit() {
        let mut inline = Inline::new(Size::new(40, 12), 0);
        let (written, area) = settle(&mut inline, &app_mid_turn());

        assert!(scrolls(&written) > 0, "got:\n{written:?}");
        assert_eq!(area.unwrap().bottom(), 12, "it sits against the bottom");
    }

    #[test]
    fn a_block_opens_where_the_cursor_is() {
        let mut inline = Inline::new(Size::new(40, 40), 7);
        let (_, area) = settle(
            &mut inline,
            &App::new("test".into(), TokenCount::new(200_000)),
        );

        assert_eq!(area.unwrap().y, 7);
    }

    #[test]
    fn a_resized_screen_places_the_block_again() {
        let mut inline = Inline::new(Size::new(40, 40), 30);
        let app = app_mid_turn();
        settle(&mut inline, &app);
        assert_eq!(settle(&mut inline, &app).1, None);

        inline.follow_screen(Size::new(30, 12));

        let (_, area) = settle(&mut inline, &app);
        let area = area.expect("the block has to be placed on the new screen");
        assert_eq!(area.width, 30);
        assert!(area.bottom() <= 12, "off the bottom: {area:?}");
    }

    #[test]
    fn the_block_comes_down_at_the_end() {
        let mut inline = Inline::new(Size::new(40, 40), 0);
        let app = app_mid_turn();
        settle(&mut inline, &app);

        let mut out = Vec::new();
        inline.finish(&app, &mut out).unwrap();
        let written = String::from_utf8_lossy(&out).into_owned();

        assert!(written.contains("live answer"), "got:\n{written}");
        assert!(
            !written.contains("settled answer"),
            "printed twice:\n{written}"
        );
        assert!(written.ends_with("\x1b[J"), "the chrome stays: {written:?}");
    }

    /// bare newlines: the ones that scroll the screen rather than end a line
    fn scrolls(written: &str) -> usize {
        written.matches('\n').count() - written.matches("\r\n").count()
    }
}
