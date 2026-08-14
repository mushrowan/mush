#[must_use]
pub fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    use ratatui::buffer::CellWidth;

    assert!(max_width > 0, "max_width must leave room for ellipsis");
    if usize::from(s.cell_width()) <= max_width {
        return s.to_string();
    }

    let budget = max_width.saturating_sub(1);
    let mut width = 0usize;
    let mut prefix = String::with_capacity(s.len());
    for ch in s.chars() {
        let mut buf = [0; 4];
        let ch_width = usize::from(ch.encode_utf8(&mut buf).cell_width());
        if width + ch_width > budget {
            break;
        }
        width += ch_width;
        prefix.push(ch);
    }
    format!("{prefix}…")
}

#[cfg(test)]
mod tests {
    use super::*;

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
