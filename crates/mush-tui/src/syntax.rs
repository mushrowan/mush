use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::theme::Theme;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

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

#[must_use]
pub fn highlight_line(line: &str, lang: &str, ui_theme: &Theme) -> Option<Vec<Span<'static>>> {
    let ps = &*SYNTAX_SET;
    let syntax = ps.find_syntax_by_token(lang)?;
    let theme = &THEME_SET.themes["base16-ocean.dark"];
    let mut h = HighlightLines::new(syntax, theme);
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
    if spans.is_empty() {
        return None;
    }
    let _ = ui_theme;
    Some(spans)
}

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

    #[test]
    fn highlight_line_styles_shell_keywords() {
        let theme = Theme::dark();
        let spans = highlight_line("for file in *.rs; do echo $file; done", "sh", &theme)
            .expect("should highlight shell");
        let styles: std::collections::HashSet<_> = spans.iter().map(|s| s.style).collect();
        assert!(
            styles.len() >= 2,
            "expected multiple styles for shell code, got {styles:?}"
        );
    }
}
