fn push_words<'a>(lead: &'static str, part: &'a str, out: &mut Vec<(&'static str, &'a str)>) {
    for (i, word) in part.split('-').enumerate() {
        out.push((if i == 0 { lead } else { "-" }, word));
    }
}

fn words(id: &str) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    match id.split_once('/') {
        Some((prefix, rest)) => {
            push_words("", prefix, &mut out);
            push_words("/", rest, &mut out);
        }
        None => push_words("", id, &mut out),
    }
    out
}

fn shorten(word: &str, keep: usize) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphabetic()) {
        word.chars().take(keep.max(1)).collect()
    } else {
        word.to_string()
    }
}

fn render(words: &[(&str, &str)], keep: &[usize]) -> String {
    words
        .iter()
        .zip(keep)
        .map(|((lead, word), keep)| format!("{lead}{}", shorten(word, *keep)))
        .collect()
}

pub(super) fn abbreviate(id: &str) -> String {
    let words = words(id);
    render(&words, &vec![1; words.len()])
}

pub(super) fn distinct(main: &str, other: &str) -> (String, String) {
    let a = words(main);
    let b = words(other);
    let mut keep_a = vec![1; a.len()];
    let mut keep_b = vec![1; b.len()];
    if main != other
        && a.len() == b.len()
        && render(&a, &keep_a) == render(&b, &keep_b)
        && let Some(i) = (0..a.len()).find(|&i| a[i].1 != b[i].1)
    {
        let longest = a[i].1.chars().count().max(b[i].1.chars().count());
        let keep = (2..=longest)
            .find(|&k| shorten(a[i].1, k) != shorten(b[i].1, k))
            .unwrap_or(longest);
        keep_a[i] = keep;
        keep_b[i] = keep;
    }
    (render(&a, &keep_a), render(&b, &keep_b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::ModelId;

    #[test]
    fn every_word_keeps_its_first_letter_and_numbers_stay_whole() {
        for id in [
            "claude-opus-5-5",
            "claude-sonnet-4",
            "openai/gpt-5.1-codex",
            "openrouter/anthropic/claude-3",
            "gpt-5.4",
            "test-model",
            "",
        ] {
            assert_eq!(abbreviate(id), ModelId::from(id).abbreviated(), "{id}");
        }
        assert_eq!(abbreviate("claude-opus-5-5"), "c-o-5-5");
    }

    #[test]
    fn two_models_that_abbreviate_apart_stay_short() {
        assert_eq!(
            distinct("claude-opus-4-5", "claude-sonnet-4-5"),
            ("c-o-4-5".to_string(), "c-s-4-5".to_string())
        );
        assert_eq!(
            distinct("claude-opus-4-5", "claude-opus-4-5"),
            ("c-o-4-5".to_string(), "c-o-4-5".to_string())
        );
    }

    #[test]
    fn a_clash_lengthens_only_the_clashing_word() {
        assert_eq!(
            distinct("claude-opus-4-5", "claude-orca-4-5"),
            ("c-op-4-5".to_string(), "c-or-4-5".to_string())
        );
        assert_eq!(
            distinct("gemini-pro", "gpt-pro"),
            ("ge-p".to_string(), "gp-p".to_string())
        );
        assert_eq!(
            distinct("gem-pro", "gemini-pro"),
            ("gem-p".to_string(), "gemi-p".to_string())
        );
    }
}
