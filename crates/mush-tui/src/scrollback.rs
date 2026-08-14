use std::io::{self, Write};

use ratatui::prelude::IntoCrossterm;
use ratatui::text::Line;

use crate::app::App;
use crate::display_types::{DisplayMessage, MessageRole};
use crate::widgets::message_list::{MessageStyle, message_lines};

pub const REWRITE_RULE: &str = " history rewritten ";

#[derive(Debug, Default)]
pub struct Scrollback {
    committed: usize,
    streamed: usize,
    settled_head: Option<SettledHead>,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
enum Cut {
    Settled,
    All,
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

    pub fn take_settled(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        width: u16,
    ) -> Vec<Line<'static>> {
        self.take(app, style, width, Cut::Settled)
    }

    pub fn take_rest(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        width: u16,
    ) -> Vec<Line<'static>> {
        self.take(app, style, width, Cut::All)
    }

    fn take(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        width: u16,
        cut: Cut,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let settled = match cut {
            Cut::Settled => app.commit_point(),
            Cut::All => app.messages.len(),
        };

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

        lines.extend(self.take_streamed(app, style, width, cut));
        lines
    }

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

    fn take_streamed(
        &mut self,
        app: &App,
        style: &MessageStyle<'_>,
        width: u16,
        cut: Cut,
    ) -> Vec<Line<'static>> {
        if !app.stream.active {
            return Vec::new();
        }

        let text = app.visible_streaming_text();
        let prefix = match cut {
            Cut::Settled => settled_prefix(text),
            Cut::All => text.len(),
        };
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
        let theme = Theme::default();
        let mut app = app();
        finish_turn(&mut app, "first question", "first answer");
        app.push_user_message("second question");

        let mut scrollback = Scrollback::new();
        scrollback.take_settled(&app, &style(&theme), 80);

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
