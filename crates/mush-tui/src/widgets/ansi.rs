//! ANSI SGR parsing for tool output previews
//!
//! bash tools often emit ANSI escape sequences (cargo errors in red,
//! jj log bookmarks in blue, eza colours, ...). rendering the raw
//! escapes as text looks like garbage, so we parse them into styled
//! ratatui spans before pushing them into the tool preview box.
//!
//! this module is intentionally small: `ansi_lines` is the single
//! entry point the renderer uses. lines with no escape sequences
//! round-trip unchanged, so the cost for typical plain output is a
//! trivial parse over ascii bytes.

use ansi_to_tui::IntoText;
use ratatui::style::Style;
use ratatui::text::Span;

/// convert a chunk of terminal output into per-line styled spans
///
/// any SGR escape sequences are parsed into `Style` attributes on the
/// resulting spans. unrecognised or malformed escapes are silently
/// dropped by the upstream parser, matching a typical terminal's
/// forgiving behaviour. plain text with no escapes returns a single
/// span per line carrying `default_style`
pub fn ansi_lines(text: &str, default_style: Style) -> Vec<Vec<Span<'static>>> {
    // fast path: no escape char at all means we can skip the parser
    if !text.contains('\x1b') {
        return text
            .lines()
            .map(|l| vec![Span::styled(l.to_string(), default_style)])
            .collect();
    }

    match text.as_bytes().into_text() {
        Ok(parsed) => parsed
            .lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|s| {
                        // treat a `Reset` colour as "no override": the box's
                        // default styling should take over, not the user's
                        // terminal default (which our embedded pane has no
                        // reason to mirror)
                        let style = merge_over_default(default_style, s.style);
                        Span::styled(s.content.into_owned(), style)
                    })
                    .collect()
            })
            .collect(),
        // parser failure is extremely rare; fall back to stripping escapes
        // naively so the user still sees readable content
        Err(_) => text
            .lines()
            .map(|l| vec![Span::styled(strip_ansi(l), default_style)])
            .collect(),
    }
}

/// overlay `sgr` onto `default_style`, treating `Color::Reset` in `sgr`
/// as "fall back to default" rather than "clear to terminal default"
fn merge_over_default(default_style: Style, sgr: Style) -> Style {
    use ratatui::style::Color;
    let mut out = default_style;
    match sgr.fg {
        Some(Color::Reset) | None => {}
        Some(c) => out.fg = Some(c),
    }
    match sgr.bg {
        Some(Color::Reset) | None => {}
        Some(c) => out.bg = Some(c),
    }
    out.add_modifier |= sgr.add_modifier;
    out.sub_modifier |= sgr.sub_modifier;
    out
}

/// remove CSI/SGR escape sequences from a line of text
///
/// used on the parser's error path and anywhere styled rendering isn't
/// feasible (e.g. side-by-side tool panels). handles the common
/// `\x1b[...m` form plus bare `\x1b` bytes
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // consume the rest of a CSI sequence up to an alpha terminator
            if chars.peek() == Some(&'[') {
                chars.next();
                for inner in chars.by_ref() {
                    if inner.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn plain_text_round_trips_with_default_style() {
        let style = Style::default().fg(Color::Gray);
        let lines = ansi_lines("hello\nworld", style);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].content, "hello");
        assert_eq!(lines[0][0].style, style);
        assert_eq!(lines[1][0].content, "world");
    }

    #[test]
    fn strips_and_styles_red_foreground() {
        let input = "\x1b[31merror\x1b[0m: something broke";
        let lines = ansi_lines(input, Style::default());
        assert_eq!(lines.len(), 1);
        let joined: String = lines[0].iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "error: something broke");
        // first span carries the red foreground
        assert_eq!(lines[0][0].style.fg, Some(Color::Red));
    }

    #[test]
    fn no_raw_escape_chars_in_output() {
        let input = "\x1b[1;32mok\x1b[0m \x1b[33mwarn\x1b[0m";
        let lines = ansi_lines(input, Style::default());
        for line in &lines {
            for span in line {
                assert!(
                    !span.content.contains('\x1b'),
                    "span still carries escape: {span:?}"
                );
            }
        }
    }

    #[test]
    fn multiline_ansi_preserves_line_split() {
        let input = "\x1b[31mone\x1b[0m\n\x1b[32mtwo\x1b[0m\nthree";
        let lines = ansi_lines(input, Style::default());
        assert_eq!(lines.len(), 3, "should have three lines: {lines:?}");
        assert_eq!(lines[0][0].content, "one");
        assert_eq!(lines[0][0].style.fg, Some(Color::Red));
        assert_eq!(lines[1][0].content, "two");
        assert_eq!(lines[1][0].style.fg, Some(Color::Green));
        assert_eq!(lines[2][0].content, "three");
    }

    #[test]
    fn default_style_is_applied_to_plain_spans() {
        let style = Style::default().fg(Color::Gray);
        let lines = ansi_lines("\x1b[31mred\x1b[0m plain", style);
        // "plain" span should inherit the default gray when no SGR set
        let plain = lines[0]
            .iter()
            .find(|s| s.content.contains("plain"))
            .expect("plain span");
        assert_eq!(plain.style.fg, Some(Color::Gray));
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        let input = "\x1b[1;31merror\x1b[0m: \x1b[33mwarn\x1b[0m here";
        assert_eq!(strip_ansi(input), "error: warn here");
    }

    #[test]
    fn strip_ansi_handles_bare_escape_without_bracket() {
        // lone ESC without a CSI intro should still be dropped
        assert_eq!(strip_ansi("pre\x1bpost"), "prepost");
    }

    #[test]
    fn strip_ansi_preserves_plain_text_unchanged() {
        let s = "hello world, no colour here";
        assert_eq!(strip_ansi(s), s);
    }
}
