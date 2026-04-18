//! fuzzy filter helper backed by [`nucleo_matcher`].
//!
//! shared between the session picker and the model picker so both get the
//! same subsequence matching + scoring behaviour. keeps a single [`Matcher`]
//! around so the internal buffers are reused across calls.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// a reusable fuzzy matcher. holds onto its internal allocations so repeated
/// filtering doesn't reallocate scratch buffers every keystroke
pub struct FuzzyFilter {
    matcher: Matcher,
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

    /// score a single haystack string against the query. returns `Some(score)`
    /// on match with higher being better, `None` if the query doesn't match.
    /// empty queries match everything with score 0 so the caller preserves
    /// the input order
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
        pattern.score(haystack, &mut self.matcher).and_then(|s| {
            // nucleo returns 0 for empty patterns which we handle above, so a
            // real match is always >0. downstream callers rely on that invariant
            if s == 0 { None } else { Some(s) }
        })
    }

    /// filter and rank a slice of items by `query`. returns indices into the
    /// original slice sorted by score descending (highest first), with the
    /// original order preserved among ties. empty query returns all indices
    /// in original order
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
        // stable sort: preserves original order for equal scores
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
        // 'helper' starts with 'help' so should rank above 'nothelp'
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
