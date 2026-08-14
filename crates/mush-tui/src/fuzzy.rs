use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

pub struct FuzzyFilter {
    matcher: Matcher,
}

impl std::fmt::Debug for FuzzyFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuzzyFilter").finish_non_exhaustive()
    }
}

impl Default for FuzzyFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl FuzzyFilter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn score(&mut self, haystack: &str, query: &str) -> Option<u32> {
        if query.is_empty() {
            return Some(0);
        }
        let mut haystack_buf = Vec::new();
        let haystack = Utf32Str::new(haystack, &mut haystack_buf);
        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        pattern
            .score(haystack, &mut self.matcher)
            .filter(|&s| s != 0)
    }

    pub fn filter<T, F>(&mut self, items: &[T], query: &str, key: F) -> Vec<usize>
    where
        F: Fn(&T) -> &str,
    {
        if query.is_empty() {
            return (0..items.len()).collect();
        }
        let mut scored: Vec<(usize, u32)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| self.score(key(item), query).map(|s| (i, s)))
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.1));
        scored.into_iter().map(|(i, _)| i).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_everything_in_order() {
        let mut f = FuzzyFilter::new();
        let items = vec!["alpha", "beta", "gamma"];
        let indices = f.filter(&items, "", |s| s);
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn subsequence_match_finds_items() {
        let mut f = FuzzyFilter::new();
        let items = vec![
            "claude-opus-4-7",
            "claude-sonnet-4-20250514",
            "gpt-5",
            "gemini-2.5-flash",
        ];
        let indices = f.filter(&items, "clop", |s| s);
        assert!(
            indices.contains(&0),
            "claude-opus-4-7 should match 'clop' as subsequence"
        );
        assert!(!indices.contains(&2), "gpt-5 should not match 'clop'");
    }

    #[test]
    fn exact_prefix_outranks_scattered_match() {
        let mut f = FuzzyFilter::new();
        let items = vec!["helper", "nothelp"];
        let indices = f.filter(&items, "help", |s| s);
        assert_eq!(indices.first(), Some(&0));
    }

    #[test]
    fn case_insensitive() {
        let mut f = FuzzyFilter::new();
        let items = vec!["Claude-Opus"];
        let indices = f.filter(&items, "claude", |s| s);
        assert_eq!(indices, vec![0]);
    }

    #[test]
    fn no_match_returns_empty() {
        let mut f = FuzzyFilter::new();
        let items = vec!["alpha", "beta"];
        let indices = f.filter(&items, "xyz", |s| s);
        assert!(indices.is_empty());
    }

    #[test]
    fn filter_by_struct_field() {
        struct Model {
            id: String,
        }
        let mut f = FuzzyFilter::new();
        let items = vec![
            Model {
                id: "claude-opus".into(),
            },
            Model { id: "gpt-5".into() },
        ];
        let indices = f.filter(&items, "opus", |m| m.id.as_str());
        assert_eq!(indices, vec![0]);
    }
}
