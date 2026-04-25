//! `@name` prompt-template trigger detection
//!
//! design pins:
//! - `@` must never shadow literal input. typing `@asdf` and hitting
//!   enter sends `@asdf` verbatim; no popup, no consumption
//! - tab is the opt-in trigger: `@asdf<tab>` tries to expand
//! - esc/ctrl+[ in the picker close without inserting (once a picker
//!   lands; today the cycle only handles exact-name tab expansion)
//!
//! this module only covers the parsing step (locate an `@word` adjacent
//! to the cursor). the tab handler in `input.rs` consumes the trigger
//! and the template picker / slot editor land in follow-up cycles.

use mush_ext::PromptTemplate;

/// a detected `@word` adjacent to the cursor.
///
/// `start` is the byte offset of the `@` sign. `word` is the
/// alphanumeric/`_` identifier that followed. an empty word (just a
/// bare `@`) is still returned so the picker can open without filter
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtTrigger {
    pub start: usize,
    pub word: String,
}

/// find an `@word` ending at `cursor` in `text`. returns `None` when
/// the cursor isn't adjacent to such a token.
///
/// the `@` must be either at the start of the string or preceded by
/// whitespace, so expressions like `email@example.com<tab>` don't fire
/// the template system. within the word only ascii alphanumeric and
/// `_` are allowed; anything else (including `-`) breaks the trigger
#[must_use]
pub fn parse_at_trigger(text: &str, cursor: usize) -> Option<AtTrigger> {
    if cursor > text.len() {
        return None;
    }
    let before = &text[..cursor];

    // scan backwards over word characters
    let word_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_at_word_char(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(cursor);

    // the byte immediately before the word must be `@`
    if word_start == 0 {
        return None;
    }
    let at_pos = before[..word_start]
        .char_indices()
        .next_back()
        .filter(|(_, c)| *c == '@')
        .map(|(i, _)| i)?;

    // the `@` must be at the string start or preceded by whitespace,
    // so `foo@bar<tab>` (email-ish) is not a trigger
    if at_pos > 0 {
        let preceding = before[..at_pos].chars().next_back()?;
        if !preceding.is_whitespace() {
            return None;
        }
    }

    Some(AtTrigger {
        start: at_pos,
        word: before[word_start..cursor].to_string(),
    })
}

fn is_at_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// find the template that should expand for a given trigger. exact
/// name match for now; fuzzy / prefix matching is a picker concern
#[must_use]
pub fn find_exact<'a>(
    templates: &'a [PromptTemplate],
    trigger: &AtTrigger,
) -> Option<&'a PromptTemplate> {
    templates.iter().find(|t| t.name == trigger.word)
}

/// candidates whose name starts with `trigger.word`. used to populate
/// the `@`-template picker when an exact match isn't available.
/// preserves source order so the picker shows templates in the same
/// order they appear in the slash menu
#[must_use]
pub fn find_prefix<'a>(
    templates: &'a [PromptTemplate],
    trigger: &AtTrigger,
) -> Vec<&'a PromptTemplate> {
    templates
        .iter()
        .filter(|t| t.name.starts_with(&trigger.word))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_at_with_cursor_at_end() {
        let trigger = parse_at_trigger("@", 1).expect("bare @ is a trigger");
        assert_eq!(trigger.start, 0);
        assert_eq!(trigger.word, "");
    }

    #[test]
    fn parse_at_word_at_start_of_input() {
        let trigger = parse_at_trigger("@review", 7).expect("at start");
        assert_eq!(trigger.start, 0);
        assert_eq!(trigger.word, "review");
    }

    #[test]
    fn parse_at_word_after_space() {
        let trigger = parse_at_trigger("hello @review", 13).expect("after space");
        assert_eq!(trigger.start, 6);
        assert_eq!(trigger.word, "review");
    }

    #[test]
    fn parse_email_shape_is_not_a_trigger() {
        // typical email address pattern must not fire a template
        assert!(parse_at_trigger("send to me@example", 18).is_none());
    }

    #[test]
    fn parse_at_word_with_cursor_mid_word_still_returns_word_so_far() {
        // cursor at position 4 (in `@rev|iew`) sees the word up to the cursor
        let trigger = parse_at_trigger("@review", 4).expect("mid word");
        assert_eq!(trigger.start, 0);
        assert_eq!(trigger.word, "rev");
    }

    #[test]
    fn parse_at_followed_by_non_word_stops_the_trigger() {
        // `@review-foo` breaks at the `-`; cursor after the dash is outside
        // any trigger, since the char before the cursor is `-`
        assert!(parse_at_trigger("@review-foo", 11).is_none());
    }

    #[test]
    fn parse_at_in_middle_of_alphanumeric_text_is_not_a_trigger() {
        // the `@` is preceded by a letter so this doesn't qualify
        assert!(parse_at_trigger("foo@bar", 7).is_none());
    }

    #[test]
    fn parse_returns_none_when_no_at_sign() {
        assert!(parse_at_trigger("hello world", 11).is_none());
    }

    #[test]
    fn find_exact_matches_template_by_name() {
        let templates = vec![PromptTemplate {
            name: "review".into(),
            description: "".into(),
            content: "content".into(),
            source: mush_ext::TemplateSource::User,
            path: std::path::PathBuf::from("/tmp/review.md"),
        }];
        let trigger = AtTrigger {
            start: 0,
            word: "review".into(),
        };
        assert_eq!(
            find_exact(&templates, &trigger).map(|t| &t.name[..]),
            Some("review")
        );
    }

    #[test]
    fn find_exact_returns_none_when_no_match() {
        let templates = Vec::new();
        let trigger = AtTrigger {
            start: 0,
            word: "missing".into(),
        };
        assert!(find_exact(&templates, &trigger).is_none());
    }

    fn make_template(name: &str) -> PromptTemplate {
        PromptTemplate {
            name: name.into(),
            description: format!("description: {name}"),
            content: format!("body: {name}"),
            source: mush_ext::TemplateSource::User,
            path: std::path::PathBuf::from(format!("/tmp/{name}.md")),
        }
    }

    #[test]
    fn find_prefix_returns_all_candidates_that_start_with_word() {
        // @rev<tab> with templates [review, review-pr, plan] should
        // surface the two `review*` ones for the picker. exact-name
        // entries are still included so the picker can let the user
        // pick the longer match if they want
        let templates = vec![
            make_template("review"),
            make_template("review-pr"),
            make_template("plan"),
        ];
        let trigger = AtTrigger {
            start: 0,
            word: "rev".into(),
        };

        let matches = find_prefix(&templates, &trigger);
        let names: Vec<&str> = matches.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["review", "review-pr"]);
    }

    #[test]
    fn find_prefix_with_empty_word_returns_everything() {
        // bare `@<tab>` is the user asking "show me all my templates"
        let templates = vec![make_template("review"), make_template("plan")];
        let trigger = AtTrigger {
            start: 0,
            word: String::new(),
        };

        let matches = find_prefix(&templates, &trigger);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn find_prefix_returns_empty_when_no_candidate_matches() {
        let templates = vec![make_template("plan")];
        let trigger = AtTrigger {
            start: 0,
            word: "review".into(),
        };
        assert!(find_prefix(&templates, &trigger).is_empty());
    }
}
