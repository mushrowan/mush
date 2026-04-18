//! shared syntect-based syntax highlighting primitives.
//!
//! used by the markdown renderer for fenced code blocks and by
//! `widgets::diff` for highlighting equal tokens in +/- lines. exposes
//! just enough surface to produce a `Vec<Span<'static>>` for a single
//! line of code given a language token.
//!
//! caveat: single-line highlighting doesn't preserve syntect's state
//! across lines (multi-line strings, block comments may lose colour
//! past the first line). acceptable for diffs where change hunks are
//! usually small and self-contained

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::theme::Theme;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// map a file path to a syntect language token via its extension.
///
/// returns `None` when the path has no extension or the extension maps
/// to a non-highlightable type. callers should fall back to plain text
#[must_use]
pub fn lang_from_path(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    Some(match ext {
        "rs" => "rs",
        "py" => "py",
        "js" | "mjs" | "cjs" => "js",
        "ts" => "ts",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kt",
        "rb" => "rb",
        "sh" | "bash" | "zsh" => "sh",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "md" | "markdown" => "md",
        "html" | "htm" => "html",
        "css" => "css",
        "nix" => "nix",
        _ => return None,
    })
}

/// highlight a single line of code via syntect, returning styled spans.
///
/// returns `None` when the language isn't known to syntect or when
/// highlighting fails. callers should fall back to plain-text rendering
#[must_use]
pub fn highlight_line(line: &str, lang: &str, ui_theme: &Theme) -> Option<Vec<Span<'static>>> {
    let ps = &*SYNTAX_SET;
    let syntax = ps.find_syntax_by_token(lang)?;
    let theme = &THEME_SET.themes["base16-ocean.dark"];
    let mut h = HighlightLines::new(syntax, theme);
    // append newline so syntect can track state within the line
    let mut line_with_nl = String::with_capacity(line.len() + 1);
    line_with_nl.push_str(line);
    line_with_nl.push('\n');
    let ranges = h.highlight_line(&line_with_nl, ps).ok()?;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(ranges.len());
    for (style, text) in ranges {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            continue;
        }
        spans.push(Span::styled(text.to_string(), syntect_to_style(style)));
    }
    // syntect may return a single dim fg range when lang matched but no
    // tokens resolved; fall back to plain in that pathological case
    if spans.is_empty() {
        return None;
    }
    let _ = ui_theme; // reserved for future theme-driven palette overrides
    Some(spans)
}

/// convert a syntect style to a ratatui `Style`. foreground is always
/// taken when opaque; background is dropped so the terminal bg shows
/// through (important for diff rows where we overlay a line-level bg)
fn syntect_to_style(style: highlighting::Style) -> Style {
    let mut s = Style::default();
    if style.foreground.a > 0 {
        s = s.fg(Color::Rgb(
            style.foreground.r,
            style.foreground.g,
            style.foreground.b,
        ));
    }
    let fs = style.font_style;
    if fs.contains(highlighting::FontStyle::BOLD) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if fs.contains(highlighting::FontStyle::ITALIC) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if fs.contains(highlighting::FontStyle::UNDERLINE) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_from_path_maps_rust() {
        assert_eq!(lang_from_path("src/main.rs"), Some("rs"));
        assert_eq!(lang_from_path("/abs/path/mod.rs"), Some("rs"));
    }

    #[test]
    fn lang_from_path_returns_none_for_unknown() {
        assert_eq!(lang_from_path("README"), None);
        assert_eq!(lang_from_path("file.xyz"), None);
    }

    #[test]
    fn highlight_line_styles_rust_keywords() {
        let theme = Theme::dark();
        let spans = highlight_line("let x = 1;", "rs", &theme).expect("should highlight");
        // should produce at least two distinct styles (keyword vs literal)
        let styles: std::collections::HashSet<_> = spans.iter().map(|s| s.style).collect();
        assert!(
            styles.len() >= 2,
            "expected multiple styles for rust code, got {styles:?}"
        );
    }

    #[test]
    fn highlight_line_returns_none_for_unknown_lang() {
        let theme = Theme::dark();
        assert!(highlight_line("x = 1", "not-a-real-lang", &theme).is_none());
    }
}
