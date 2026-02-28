//! lightweight markdown to ratatui text renderer
//!
//! handles the common patterns in LLM output: headings, bold, italic,
//! inline code, code blocks, lists, and horizontal rules. not a full
//! markdown parser, just enough for readable agent output.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

/// render a markdown string to styled ratatui Text
pub fn render(source: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_block_lines: Vec<String> = Vec::new();

    for raw_line in source.lines() {
        if raw_line.starts_with("```") {
            if in_code_block {
                // end code block
                for code_line in &code_block_lines {
                    lines.push(Line::styled(
                        format!("  {code_line}"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                code_block_lines.clear();
                in_code_block = false;
            } else {
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            code_block_lines.push(raw_line.to_string());
            continue;
        }

        // headings
        if let Some(rest) = raw_line.strip_prefix("### ") {
            lines.push(Line::styled(
                rest.to_string(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix("## ") {
            lines.push(Line::styled(
                rest.to_string(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix("# ") {
            lines.push(Line::styled(
                rest.to_string(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
            continue;
        }

        // horizontal rule
        if raw_line == "---" || raw_line == "***" || raw_line == "___" {
            lines.push(Line::styled(
                "─".repeat(40),
                Style::default().fg(Color::DarkGray),
            ));
            continue;
        }

        // list items
        if let Some(rest) = raw_line
            .strip_prefix("- ")
            .or_else(|| raw_line.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled("• ", Style::default().fg(Color::Cyan))];
            spans.extend(parse_inline(rest));
            lines.push(Line::from(spans));
            continue;
        }

        // numbered list items
        if let Some(dot_pos) = raw_line.find(". ")
            && dot_pos <= 3
            && raw_line[..dot_pos].chars().all(|c| c.is_ascii_digit())
        {
            let prefix = &raw_line[..=dot_pos];
            let rest = &raw_line[dot_pos + 2..];
            let mut spans = vec![Span::styled(
                format!("{prefix} "),
                Style::default().fg(Color::Cyan),
            )];
            spans.extend(parse_inline(rest));
            lines.push(Line::from(spans));
            continue;
        }

        // regular paragraph with inline formatting
        if raw_line.is_empty() {
            lines.push(Line::raw(""));
        } else {
            lines.push(Line::from(parse_inline(raw_line)));
        }
    }

    // close any unclosed code block
    if in_code_block {
        for code_line in &code_block_lines {
            lines.push(Line::styled(
                format!("  {code_line}"),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    Text::from(lines)
}

/// parse inline markdown: **bold**, *italic*, `code`, ***bold italic***
fn parse_inline(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut buf = String::new();

    while let Some((i, ch)) = chars.next() {
        match ch {
            '`' => {
                // inline code
                if !buf.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut buf)));
                }
                let mut code = String::new();
                for (_, c) in chars.by_ref() {
                    if c == '`' {
                        break;
                    }
                    code.push(c);
                }
                spans.push(Span::styled(
                    code,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            '*' | '_' => {
                // check for bold/italic
                let marker = ch;
                let remaining = &text[i..];
                if remaining.starts_with("***") || remaining.starts_with("___") {
                    // bold italic
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    chars.next(); // skip 2nd
                    chars.next(); // skip 3rd
                    let closing = format!("{marker}{marker}{marker}");
                    let mut inner = String::new();
                    for (_, c) in chars.by_ref() {
                        let rest: String = inner.clone() + &c.to_string();
                        if rest.ends_with(&closing) {
                            inner = rest[..rest.len() - 3].to_string();
                            break;
                        }
                        inner.push(c);
                    }
                    spans.push(Span::styled(
                        inner,
                        Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    ));
                } else if remaining.starts_with("**") || remaining.starts_with("__") {
                    // bold
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    chars.next(); // skip 2nd
                    let closing = format!("{marker}{marker}");
                    let mut inner = String::new();
                    for (_, c) in chars.by_ref() {
                        let rest: String = inner.clone() + &c.to_string();
                        if rest.ends_with(&closing) {
                            inner = rest[..rest.len() - 2].to_string();
                            break;
                        }
                        inner.push(c);
                    }
                    spans.push(Span::styled(
                        inner,
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                } else {
                    // italic (single marker)
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    let mut inner = String::new();
                    let mut found = false;
                    for (_, c) in chars.by_ref() {
                        if c == marker {
                            found = true;
                            break;
                        }
                        inner.push(c);
                    }
                    if found {
                        spans.push(Span::styled(
                            inner,
                            Style::default().add_modifier(Modifier::ITALIC),
                        ));
                    } else {
                        // no closing marker, treat as literal
                        buf.push(marker);
                        buf.push_str(&inner);
                    }
                }
            }
            _ => buf.push(ch),
        }
    }

    if !buf.is_empty() {
        spans.push(Span::raw(buf));
    }

    if spans.is_empty() {
        spans.push(Span::raw(""));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        let text = render("hello world");
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "hello world");
    }

    #[test]
    fn headings() {
        let text = render("# heading 1\n## heading 2\n### heading 3");
        assert_eq!(text.lines.len(), 3);
        assert_eq!(text.lines[0].to_string(), "heading 1");
        assert_eq!(text.lines[1].to_string(), "heading 2");
        assert_eq!(text.lines[2].to_string(), "heading 3");
    }

    #[test]
    fn bold_text() {
        let text = render("some **bold** text");
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "some bold text");
        // the middle span should be bold
        assert_eq!(text.lines[0].spans.len(), 3);
        assert!(
            text.lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn italic_text() {
        let text = render("some *italic* text");
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].spans.len(), 3);
        assert!(
            text.lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn inline_code() {
        let text = render("use `cargo build`");
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "use cargo build");
    }

    #[test]
    fn code_block() {
        let text = render("```rust\nfn main() {}\n```");
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "  fn main() {}");
    }

    #[test]
    fn unordered_list() {
        let text = render("- item one\n- item two");
        assert_eq!(text.lines.len(), 2);
        assert!(text.lines[0].to_string().contains("item one"));
        assert!(text.lines[1].to_string().contains("item two"));
    }

    #[test]
    fn numbered_list() {
        let text = render("1. first\n2. second");
        assert_eq!(text.lines.len(), 2);
        assert!(text.lines[0].to_string().contains("first"));
    }

    #[test]
    fn horizontal_rule() {
        let text = render("above\n---\nbelow");
        assert_eq!(text.lines.len(), 3);
        assert!(text.lines[1].to_string().contains("─"));
    }

    #[test]
    fn empty_input() {
        let text = render("");
        assert!(text.lines.is_empty());
    }

    #[test]
    fn mixed_content() {
        let text = render("# title\n\nsome **bold** and `code`\n\n- a list item");
        assert!(text.lines.len() >= 4);
    }
}
