use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self, ThemeSet};
use syntect::parsing::SyntaxSet;

pub(crate) static SYNTAX_SET: LazyLock<SyntaxSet> =
    LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

const HIGHLIGHT_MEMO_LINES: usize = 4096;
const HIGHLIGHT_MEMO_BLOCKS: usize = 256;
const HIGHLIGHT_MEMO_MAX_LINE_BYTES: usize = 512;

type Spans = Vec<Span<'static>>;

thread_local! {
    static HIGHLIGHTED_LINES: RefCell<Memo<Option<Spans>>> =
        RefCell::new(Memo::new(HIGHLIGHT_MEMO_LINES));
    static HIGHLIGHTED_BLOCKS: RefCell<Memo<Vec<Option<Spans>>>> =
        RefCell::new(Memo::new(HIGHLIGHT_MEMO_BLOCKS));
}

pub(crate) struct Memo<V> {
    young: HashMap<String, V>,
    old: HashMap<String, V>,
    capacity: usize,
}

impl<V: Clone> Memo<V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            young: HashMap::new(),
            old: HashMap::new(),
            capacity,
        }
    }

    pub(crate) fn get(&mut self, key: &str) -> Option<V> {
        if let Some(value) = self.young.get(key) {
            return Some(value.clone());
        }
        let value = self.old.remove(key)?;
        self.insert(key.to_string(), value.clone());
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: String, value: V) {
        if self.young.len() >= self.capacity {
            self.old = std::mem::take(&mut self.young);
        }
        self.young.insert(key, value);
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.young.contains_key(key) || self.old.contains_key(key)
    }
}

pub(crate) fn code_theme() -> &'static highlighting::Theme {
    &THEME_SET.themes["base16-ocean.dark"]
}

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
pub fn highlight_line(line: &str, lang: &str) -> Option<Vec<Span<'static>>> {
    if line.len() > HIGHLIGHT_MEMO_MAX_LINE_BYTES {
        return highlight_line_uncached(line, lang);
    }
    let key = format!("{lang}\0{line}");
    if let Some(spans) = HIGHLIGHTED_LINES.with_borrow_mut(|memo| memo.get(&key)) {
        return spans;
    }
    let spans = highlight_line_uncached(line, lang);
    HIGHLIGHTED_LINES.with_borrow_mut(|memo| memo.insert(key, spans.clone()));
    spans
}

#[must_use]
pub fn highlight_block(code: &str, lang: &str) -> Option<Vec<Option<Spans>>> {
    let key = format!("{lang}\0{code}");
    if let Some(lines) = HIGHLIGHTED_BLOCKS.with_borrow_mut(|memo| memo.get(&key)) {
        return Some(lines);
    }
    let ps = &*SYNTAX_SET;
    let syntax = ps.find_syntax_by_token(lang)?;
    let mut highlighter = HighlightLines::new(syntax, code_theme());
    let mut line_with_nl = String::new();
    let lines: Vec<Option<Spans>> = code
        .lines()
        .map(|line| {
            line_with_nl.clear();
            line_with_nl.push_str(line);
            line_with_nl.push('\n');
            let ranges = highlighter.highlight_line(&line_with_nl, ps).ok()?;
            let spans: Spans = ranges
                .into_iter()
                .map(|(style, text)| (style, text.trim_end_matches('\n')))
                .filter(|(_, text)| !text.is_empty())
                .map(|(style, text)| Span::styled(text.to_string(), syntect_to_style(style)))
                .collect();
            (!spans.is_empty()).then_some(spans)
        })
        .collect();
    HIGHLIGHTED_BLOCKS.with_borrow_mut(|memo| memo.insert(key, lines.clone()));
    Some(lines)
}

fn highlight_line_uncached(line: &str, lang: &str) -> Option<Vec<Span<'static>>> {
    let ps = &*SYNTAX_SET;
    let syntax = ps.find_syntax_by_token(lang)?;
    let mut h = HighlightLines::new(syntax, code_theme());
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
    Some(spans)
}

pub(crate) fn syntect_to_style(style: highlighting::Style) -> Style {
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
        let spans = highlight_line("let x = 1;", "rs").expect("should highlight");
        let styles: std::collections::HashSet<_> = spans.iter().map(|s| s.style).collect();
        assert!(
            styles.len() >= 2,
            "expected multiple styles for rust code, got {styles:?}"
        );
    }

    #[test]
    fn a_line_highlighted_again_comes_back_the_same() {
        let first = highlight_line("let x = 1;", "rs");
        let again = highlight_line("let x = 1;", "rs");

        assert_eq!(first, again);
        assert!(HIGHLIGHTED_LINES.with_borrow(|memo| memo.contains("rs\0let x = 1;")));
    }

    #[test]
    fn a_block_keeps_its_parser_state_across_lines() {
        let code = "x = \"\"\"\nstill inside\n\"\"\"\ny = 1";
        let lines = highlight_block(code, "py").expect("python highlights");
        let alone = highlight_line("still inside", "py").expect("a line highlights");

        assert_eq!(lines.len(), 4);
        let inside = lines[1].as_ref().expect("the docstring line");
        assert_eq!(
            inside
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "still inside"
        );
        assert_ne!(
            inside[0].style, alone[0].style,
            "the string colour carries over"
        );
        assert_eq!(highlight_block(code, "py"), Some(lines));
    }

    #[test]
    fn a_full_memo_keeps_what_was_used_recently() {
        let mut memo = Memo::new(2);
        memo.insert("a".into(), 1);
        memo.insert("b".into(), 2);
        memo.insert("c".into(), 3);
        assert_eq!(memo.get("a"), Some(1));
        memo.insert("d".into(), 4);
        memo.insert("e".into(), 5);

        assert_eq!(memo.get("a"), Some(1));
        assert_eq!(memo.get("b"), None);
        assert_eq!(memo.get("e"), Some(5));
    }

    #[test]
    fn highlight_line_returns_none_for_unknown_lang() {
        assert!(highlight_line("x = 1", "not-a-real-lang").is_none());
    }

    #[test]
    fn highlight_line_styles_shell_keywords() {
        let spans = highlight_line("for file in *.rs; do echo $file; done", "sh")
            .expect("should highlight shell");
        let styles: std::collections::HashSet<_> = spans.iter().map(|s| s.style).collect();
        assert!(
            styles.len() >= 2,
            "expected multiple styles for shell code, got {styles:?}"
        );
    }
}
