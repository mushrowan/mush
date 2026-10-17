use ratatui::buffer::CellWidth;
use unicode_segmentation::UnicodeSegmentation;

#[must_use]
pub fn grapheme_width(grapheme: &str) -> usize {
    match grapheme.as_bytes() {
        [byte] => usize::from(!byte.is_ascii_control()),
        _ if grapheme.contains(char::is_control) => 0,
        _ => usize::from(grapheme.cell_width()),
    }
}

#[must_use]
pub fn width(text: &str) -> usize {
    if text.is_ascii() {
        return text.bytes().filter(|byte| !byte.is_ascii_control()).count();
    }
    text.graphemes(true).map(grapheme_width).sum()
}

fn one_byte_graphemes(text: &str) -> bool {
    text.is_ascii() && !text.contains('\r')
}

pub fn graphemes(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let ascii = one_byte_graphemes(text);
    let bytes = ascii.then(|| (0..text.len()).map(move |at| (at, &text[at..=at])));
    let clusters = (!ascii).then(|| text.grapheme_indices(true));
    bytes
        .into_iter()
        .flatten()
        .chain(clusters.into_iter().flatten())
}

#[must_use]
pub fn grapheme_count(text: &str) -> usize {
    if one_byte_graphemes(text) {
        text.len()
    } else {
        text.graphemes(true).count()
    }
}

#[must_use]
pub fn byte_of_grapheme(text: &str, index: usize) -> usize {
    graphemes(text)
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

#[must_use]
pub fn grapheme_at_byte(text: &str, byte: usize) -> usize {
    graphemes(text).take_while(|(at, _)| *at < byte).count()
}

#[must_use]
pub fn width_before_grapheme(text: &str, index: usize) -> usize {
    graphemes(text)
        .take(index)
        .map(|(_, grapheme)| grapheme_width(grapheme))
        .sum()
}

#[must_use]
pub fn prefix_within(text: &str, max_width: usize) -> &str {
    let mut used = 0;
    for (at, grapheme) in graphemes(text) {
        let next = used + grapheme_width(grapheme);
        if next > max_width {
            return &text[..at];
        }
        used = next;
    }
    text
}

pub struct LowercaseNeedle<'a> {
    needle: &'a str,
    ascii: Option<regex::Regex>,
}

impl<'a> LowercaseNeedle<'a> {
    #[must_use]
    pub fn new(needle: &'a str) -> Self {
        let ascii = needle
            .is_ascii()
            .then(|| {
                regex::RegexBuilder::new(&regex::escape(needle))
                    .case_insensitive(true)
                    .unicode(false)
                    .build()
                    .ok()
            })
            .flatten();
        Self { needle, ascii }
    }

    #[must_use]
    pub fn in_text(&self, text: &str) -> bool {
        match &self.ascii {
            Some(regex) if text.is_ascii() => regex.is_match(text),
            _ => text.to_lowercase().contains(self.needle),
        }
    }
}

#[must_use]
pub fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    assert!(max_width > 0, "max_width must leave room for ellipsis");
    if width(s) <= max_width {
        return s.to_string();
    }
    format!("{}…", prefix_within(s, max_width.saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    const FLAG: &str = "\u{1f1ef}\u{1f1f5}";
    const COMBINED_GA: &str = "\u{304b}\u{3099}";
    const ACUTE_E: &str = "e\u{301}";

    #[test]
    fn widths_follow_the_cells_a_terminal_draws() {
        assert_eq!(width("中文"), 4);
        assert_eq!(width("日本語のテキスト"), 16);
        assert_eq!(width(COMBINED_GA), 2);
        assert_eq!(width("ｶﾞ"), 2);
        assert_eq!(width("한국어"), 6);
        assert_eq!(width(FAMILY), 2);
        assert_eq!(width(FLAG), 2);
        assert_eq!(width("\u{2764}\u{fe0f}"), 2);
        assert_eq!(width(ACUTE_E), 1);
        assert_eq!(width("a\tb"), 2);
    }

    #[test]
    fn graphemes_keep_clusters_whole() {
        let text = format!("{ACUTE_E}x{FAMILY}{FLAG}{COMBINED_GA}");
        let clusters: Vec<&str> = graphemes(&text).map(|(_, grapheme)| grapheme).collect();
        assert_eq!(clusters, [ACUTE_E, "x", FAMILY, FLAG, COMBINED_GA]);
        assert_eq!(graphemes("a\r\nb").count(), 3);
        assert_eq!(grapheme_count(&format!("{FAMILY}{FLAG}")), 2);
    }

    #[test]
    fn grapheme_positions_map_to_bytes_and_cells() {
        let text = format!("中{FAMILY}e{ACUTE_E}");
        assert_eq!(byte_of_grapheme(&text, 1), "中".len());
        assert_eq!(byte_of_grapheme(&text, 9), text.len());
        assert_eq!(grapheme_at_byte(&text, "中".len() + FAMILY.len()), 2);
        assert_eq!(width_before_grapheme(&text, 3), 5);
    }

    #[test]
    fn a_prefix_never_splits_a_wide_or_joined_grapheme() {
        assert_eq!(prefix_within("中文字", 5), "中文");
        assert_eq!(prefix_within(&format!("{FAMILY}ab"), 1), "");
        assert_eq!(prefix_within(&format!("{ACUTE_E}{ACUTE_E}"), 1), ACUTE_E);
        assert_eq!(truncate_with_ellipsis("中文字幕", 5), "中文…");
        assert_eq!(truncate_with_ellipsis(&format!("ab{FLAG}{FLAG}"), 4), "ab…");
        assert_eq!(
            truncate_with_ellipsis(&format!("ab{FLAG}{FLAG}"), 5),
            format!("ab{FLAG}…")
        );
    }

    #[test]
    fn short_returns_input() {
        assert_eq!(truncate_with_ellipsis("hi", 60), "hi");
    }

    #[test]
    fn long_truncates_with_single_ellipsis() {
        let s = "x".repeat(100);
        let out = truncate_with_ellipsis(&s, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
        assert!(!out.contains("..."));
    }

    #[test]
    fn unicode_input_truncates_by_display_width() {
        let s = "café🍓🍓🍓🍓";
        let out = truncate_with_ellipsis(s, 4);
        assert_eq!(out, "caf…");
        assert_eq!(unicode_width::UnicodeWidthStr::width(out.as_str()), 4);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn wide_unicode_truncates_by_display_width() {
        let out = truncate_with_ellipsis("abcdefg🍓", 8);
        assert_eq!(out, "abcdefg…");
        assert_eq!(unicode_width::UnicodeWidthStr::width(out.as_str()), 8);
    }

    #[test]
    fn wide_unicode_exact_boundary_preserved() {
        assert_eq!(truncate_with_ellipsis("abcdef🍓", 8), "abcdef🍓");
    }

    #[test]
    fn halfwidth_dakuten_uses_terminal_cell_width() {
        let s = "abcｶﾞ";
        assert_eq!(truncate_with_ellipsis(s, 4), "abc…");
    }

    #[test]
    fn exactly_at_boundary_preserved() {
        let s: String = "x".repeat(10);
        assert_eq!(truncate_with_ellipsis(&s, 10), s);
    }
}
