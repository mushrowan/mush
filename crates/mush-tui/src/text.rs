//! shared text helpers for rendering previews
//!
//! keep width-aware truncation logic here so display code stays declarative
//! and consistent about what "truncated" looks like

/// truncate `s` to at most `max_chars` grapheme-ish units
/// (counted by `chars()`), appending `…` when truncation happened.
/// the resulting string has at most `max_chars` scalar chars, with the
/// final char being `…` on truncation. returns an owned string in both
/// cases for simple call sites
///
/// panics if `max_chars == 0` because there's no room for even the `…`
#[must_use]
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    assert!(max_chars > 0, "max_chars must leave room for ellipsis");
    if s.chars().count() > max_chars {
        let prefix: String = s.chars().take(max_chars - 1).collect();
        format!("{prefix}…")
    } else {
        s.to_string()
    }
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
    fn unicode_input_truncates_by_char_count() {
        // four chars: café🍓 (e-acute + emoji)
        let s = "café🍓🍓🍓🍓";
        let out = truncate_with_ellipsis(s, 4);
        assert_eq!(out.chars().count(), 4);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn exactly_at_boundary_preserved() {
        let s: String = "x".repeat(10);
        assert_eq!(truncate_with_ellipsis(&s, 10), s);
    }
}
