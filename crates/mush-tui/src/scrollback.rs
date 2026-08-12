//! what the inline viewport has already handed to the terminal

use std::io::{self, Write};

use ratatui::prelude::IntoCrossterm;
use ratatui::text::Line;

use crate::app::App;
use crate::display_types::{DisplayMessage, MessageRole};
use crate::widgets::message_list::{MessageStyle, message_lines};

/// label on the rule that separates two runs of committed history
pub const REWRITE_RULE: &str = " history rewritten ";

/// tracks what has gone to the terminal's scrollback, so each line is
/// printed exactly once and never printed again
#[derive(Debug, Default)]
pub struct Scrollback {
    committed: usize,
    /// bytes of the in-flight reply already handed over
    streamed: usize,
    /// the message the in-flight reply settled into, and how much of it
    /// went out early
    settled_head: Option<SettledHead>,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct SettledHead {
    message: usize,
    bytes: usize,
}

impl Scrollback {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// lines for everything that has settled since the last call.
    ///
    /// a long reply hands over its head while its tail is still arriving,
    /// since the live region can't outgrow the terminal. a rewrite
    /// (`/compact`, `/branch`, `/undo`) rebuilds `app.messages` from the
    /// conversation tree, and the terminal can't retract what it already
    /// holds, so it draws a rule and starts a fresh run below it
    pub fn take_settled(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        width: u16,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let settled = app.commit_point();

        if app.display_generation() != self.generation {
            if self.committed > 0 || self.streamed > 0 {
                lines.push(rewrite_rule(style, width));
            }
            self.generation = app.display_generation();
            self.committed = 0;
            self.streamed = 0;
            self.settled_head = None;
        }

        self.claim_settled_head(app);

        for (i, msg) in app
            .messages
            .iter()
            .enumerate()
            .take(settled)
            .skip(self.committed)
        {
            match self.settled_head.take_if(|head| head.message == i) {
                Some(head) => lines.extend(message_lines(&tail_of(msg, head.bytes), style, width)),
                None => lines.extend(message_lines(msg, style, width)),
            }
            lines.push(Line::raw(""));
        }
        self.committed = settled;

        lines.extend(self.take_streamed(app, style, width));
        lines
    }

    /// note which message the in-flight reply settled into, so its head
    /// isn't handed over a second time
    fn claim_settled_head(&mut self, app: &App) {
        if app.stream.active || self.streamed == 0 {
            return;
        }
        self.settled_head = app
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::Assistant && !m.queued)
            .map(|message| SettledHead {
                message,
                bytes: self.streamed,
            });
        self.streamed = 0;
    }

    /// the blocks of the in-flight reply that can no longer reshape
    fn take_streamed(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        width: u16,
    ) -> Vec<Line<'static>> {
        if !app.stream.active {
            return Vec::new();
        }

        let text = app.visible_streaming_text();
        let prefix = settled_prefix(text);
        if prefix <= self.streamed {
            return Vec::new();
        }

        let head = DisplayMessage {
            thinking: (self.streamed == 0)
                .then(|| app.stream.thinking.clone())
                .filter(|t| !t.is_empty()),
            ..DisplayMessage::new(MessageRole::Assistant, &text[self.streamed..prefix])
        };
        self.streamed = prefix;
        message_lines(&head, style, width)
    }
}

/// the part of `msg` scrollback hasn't seen: everything but the first
/// `head` bytes of its content, and no thinking, which went out with them
fn tail_of(msg: &DisplayMessage, head: usize) -> DisplayMessage {
    DisplayMessage {
        thinking: None,
        content: msg.content.get(head..).unwrap_or_default().to_string(),
        ..msg.clone()
    }
}

fn rewrite_rule(style: &MessageStyle<'_>, width: u16) -> Line<'static> {
    let dashes = (width as usize).saturating_sub(REWRITE_RULE.len()) / 2;
    let rule = "─".repeat(dashes);
    Line::styled(format!("{rule}{REWRITE_RULE}{rule}"), style.theme.dim)
}

/// print settled lines above the live region, where they become the
/// terminal's own scrollback.
///
/// raw mode is on, so each line ends `\r\n`
pub fn write_lines(out: &mut impl Write, lines: &[Line<'_>]) -> io::Result<()> {
    for line in lines {
        for span in &line.spans {
            let style = line.style.patch(span.style).into_crossterm();
            write!(out, "{}", style.apply(&span.content))?;
        }
        write!(out, "\r\n")?;
    }
    Ok(())
}

/// byte offset up to which streamed markdown can no longer reshape.
///
/// the live region can't outgrow the terminal, so a long reply has to
/// commit its head while its tail is still arriving. a blank line ends a
/// markdown block, so everything before the last blank line is final. a
/// fence that hasn't closed holds back everything from its opening line,
/// since the text inside it isn't a block yet
#[must_use]
pub fn settled_prefix(text: &str) -> usize {
    let mut fence: Option<&str> = None;
    let mut settled = 0;
    let mut offset = 0;

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        offset += line.len();
        match fence {
            Some(marker) if trimmed.starts_with(marker) => fence = None,
            Some(_) => {}
            None => {
                if let Some(marker) = fence_marker(trimmed) {
                    fence = Some(marker);
                } else if trimmed.is_empty() {
                    settled = offset;
                }
            }
        }
    }
    settled
}

/// the run of backticks or tildes that opens a fence, if this line does
fn fence_marker(trimmed: &str) -> Option<&'static str> {
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use mush_ai::types::TokenCount;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    fn app() -> App {
        App::new("test".into(), TokenCount::new(200_000))
    }

    fn finish_turn(app: &mut App, prompt: &str, reply: &str) {
        app.push_user_message(prompt);
        app.start_streaming();
        app.push_text_delta(reply);
        app.finish_streaming(None, None);
    }

    /// reveal everything the typewriter is holding back
    fn reveal(app: &mut App) {
        for _ in 0..200 {
            app.tick();
        }
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn style(theme: &Theme) -> MessageStyle<'_> {
        MessageStyle {
            theme,
            show_usage: false,
        }
    }

    #[test]
    fn nothing_settles_before_the_first_turn_ends() {
        let theme = Theme::default();
        let mut app = app();
        app.push_user_message("hello");
        let mut scrollback = Scrollback::new();
        assert!(scrollback.take_settled(&app, &style(&theme), 80).is_empty());
    }

    #[test]
    fn a_settled_turn_is_handed_over_once() {
        let theme = Theme::default();
        let mut app = app();
        finish_turn(&mut app, "first question", "first answer");
        app.push_user_message("second question");

        let mut scrollback = Scrollback::new();
        let first = scrollback.take_settled(&app, &style(&theme), 80);
        assert!(text_of(&first).contains("first answer"));

        let second = scrollback.take_settled(&app, &style(&theme), 80);
        assert!(
            second.is_empty(),
            "already committed history must not be handed over twice"
        );
    }

    #[test]
    fn every_message_is_printed_exactly_once_across_a_session() {
        let theme = Theme::default();
        let mut app = app();
        let mut scrollback = Scrollback::new();
        let mut printed = String::new();

        for turn in 0..4 {
            finish_turn(
                &mut app,
                &format!("question {turn}"),
                &format!("answer {turn}"),
            );
            printed.push_str(&text_of(&scrollback.take_settled(&app, &style(&theme), 80)));
        }

        for turn in 0..3 {
            assert_eq!(
                printed.matches(&format!("answer {turn}")).count(),
                1,
                "answer {turn} should reach scrollback exactly once, got:\n{printed}"
            );
        }
    }

    #[test]
    fn a_rewrite_draws_a_rule_and_starts_again() {
        // the rewritten conversation lands on the same commit point as the
        // one it replaced, so only the generation tells them apart
        let theme = Theme::default();
        let mut app = app();
        finish_turn(&mut app, "first question", "first answer");
        app.push_user_message("second question");

        let mut scrollback = Scrollback::new();
        scrollback.take_settled(&app, &style(&theme), 80);

        // /compact rebuilds the display from the conversation tree
        app.clear_display();
        finish_turn(&mut app, "summary", "compacted");
        app.push_user_message("carry on");

        let after = text_of(&scrollback.take_settled(&app, &style(&theme), 80));
        assert!(after.contains(REWRITE_RULE), "got:\n{after}");
        assert!(after.contains("compacted"));
    }

    #[test]
    fn a_growing_paragraph_settles_nothing() {
        assert_eq!(settled_prefix("a partial sen"), 0);
    }

    #[test]
    fn a_finished_block_settles() {
        let text = "first para\n\nsecond par";
        assert_eq!(&text[..settled_prefix(text)], "first para\n\n");
    }

    #[test]
    fn an_open_fence_holds_back_everything_from_its_opening() {
        let text = "intro\n\n```rust\nfn main() {\n";
        assert_eq!(&text[..settled_prefix(text)], "intro\n\n");
    }

    #[test]
    fn a_closed_fence_settles_with_the_blank_line_after_it() {
        let text = "```rust\nfn main() {}\n```\n\nafter";
        assert_eq!(
            &text[..settled_prefix(text)],
            "```rust\nfn main() {}\n```\n\n"
        );
    }

    #[test]
    fn a_blank_line_inside_a_fence_does_not_settle() {
        let text = "```\none\n\ntwo\n";
        assert_eq!(settled_prefix(text), 0);
    }

    #[test]
    fn a_setext_heading_cannot_rewrite_settled_text() {
        // the ==== retroactively turns the line above into a heading, so
        // that line must not have been committed already
        let text = "para\n\nheading\n====\n";
        assert_eq!(&text[..settled_prefix(text)], "para\n\n");
    }

    #[test]
    fn the_prefix_only_grows_as_the_reply_streams() {
        let full = "intro para\n\n```rust\nfn main() {\n    todo!()\n}\n```\n\nmiddle bit\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\ndone\n";
        let mut seen = 0;
        for end in 0..=full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            let prefix = settled_prefix(&full[..end]);
            assert!(
                prefix >= seen,
                "settled prefix went backwards at {end}: {prefix} < {seen}"
            );
            assert!(full.is_char_boundary(prefix));
            seen = prefix;
        }
        assert!(seen > 0, "a finished reply should settle something");
    }

    #[test]
    fn a_settled_paragraph_goes_out_while_the_rest_streams() {
        let theme = Theme::default();
        let mut app = app();
        app.push_user_message("hello");
        app.start_streaming();
        app.push_text_delta("first block\n\nstill wri");
        reveal(&mut app);

        let mut scrollback = Scrollback::new();
        let out = text_of(&scrollback.take_settled(&app, &style(&theme), 80));
        assert!(out.contains("first block"), "got:\n{out}");
        assert!(!out.contains("still wri"), "got:\n{out}");
    }

    #[test]
    fn a_settled_paragraph_goes_out_once() {
        let theme = Theme::default();
        let mut app = app();
        app.push_user_message("hello");
        app.start_streaming();
        app.push_text_delta("first block\n\nsecond bl");
        reveal(&mut app);

        let mut scrollback = Scrollback::new();
        scrollback.take_settled(&app, &style(&theme), 80);
        app.push_text_delta("ock\n\nthird");
        reveal(&mut app);
        let out = text_of(&scrollback.take_settled(&app, &style(&theme), 80));

        assert!(out.contains("second block"), "got:\n{out}");
        assert!(!out.contains("first block"), "already handed over:\n{out}");
    }

    #[test]
    fn the_settled_head_is_not_repeated_when_the_turn_lands() {
        let theme = Theme::default();
        let mut app = app();
        app.push_user_message("hello");
        app.start_streaming();
        app.push_text_delta("first block\n\nsecond block\n");
        reveal(&mut app);

        let mut scrollback = Scrollback::new();
        let early = text_of(&scrollback.take_settled(&app, &style(&theme), 80));
        assert!(early.contains("first block"));

        app.finish_streaming(None, None);
        app.push_user_message("next");
        let late = text_of(&scrollback.take_settled(&app, &style(&theme), 80));

        assert!(!late.contains("first block"), "printed twice:\n{late}");
        assert!(late.contains("second block"), "got:\n{late}");
    }

    fn written(lines: &[Line<'static>]) -> String {
        let mut out = Vec::new();
        write_lines(&mut out, lines).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_plain_line_writes_its_text_and_a_newline() {
        assert_eq!(written(&[Line::raw("hello")]), "hello\r\n");
    }

    #[test]
    fn an_empty_line_writes_a_newline() {
        assert_eq!(written(&[Line::raw("")]), "\r\n");
    }

    #[test]
    fn a_styled_span_writes_an_escape_and_resets() {
        let out = written(&[Line::from(Span::styled(
            "red",
            Style::default().fg(Color::Red),
        ))]);
        let (before, after) = out.split_once("red").expect("the text should be there");
        assert!(before.starts_with('\x1b'), "{out:?}");
        assert!(
            after.starts_with('\x1b'),
            "the colour should not leak: {out:?}"
        );
        assert!(out.ends_with("\r\n"), "{out:?}");
    }

    #[test]
    fn a_line_style_reaches_a_bare_span() {
        let plain = written(&[Line::raw("x")]);
        let styled = written(&[Line::raw("x").style(Style::default().fg(Color::Blue))]);
        assert_ne!(plain, styled);
    }
}
