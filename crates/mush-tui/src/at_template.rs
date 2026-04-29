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

/// a single `$N` / `$@` / `$ARGUMENTS` placeholder occurrence inside
/// template content. produced by [`find_placeholders`] for the slot
/// editor: each placeholder becomes an empty slot the user fills in
/// interactively after the template expands
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    /// byte offset of the leading `$` in the template content
    pub start: usize,
    /// total length in bytes (`$1` = 2, `$ARGUMENTS` = 10)
    pub len: usize,
}

/// scan template content for `$1`..`$9`, `$@`, and `$ARGUMENTS`
/// placeholders and return them in source order. uses the same syntax
/// as [`mush_ext::substitute_args`] so non-interactive `/cmd arg1 arg2`
/// invocations and the interactive slot editor share placeholder
/// semantics
#[must_use]
pub fn find_placeholders(content: &str) -> Vec<Placeholder> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let next = bytes.get(i + 1).copied();
        match next {
            Some(b'1'..=b'9') | Some(b'@') => {
                out.push(Placeholder { start: i, len: 2 });
                i += 2;
            }
            Some(b'A') if bytes.get(i..i + 10) == Some(b"$ARGUMENTS") => {
                out.push(Placeholder { start: i, len: 10 });
                i += 10;
            }
            _ => i += 1,
        }
    }
    out
}

/// remove every `$N` / `$@` / `$ARGUMENTS` placeholder from `content`
/// and return the cleaned text together with the byte offsets where
/// each slot lived in the cleaned text. these offsets are what the
/// slot editor jumps the cursor between
#[must_use]
pub fn strip_placeholders(content: &str) -> (String, Vec<usize>) {
    let placeholders = find_placeholders(content);
    if placeholders.is_empty() {
        return (content.to_string(), Vec::new());
    }
    let mut clean = String::with_capacity(content.len());
    let mut slots = Vec::with_capacity(placeholders.len());
    let mut prev_end = 0usize;
    for placeholder in &placeholders {
        clean.push_str(&content[prev_end..placeholder.start]);
        // slot lands at the current end of `clean`, where the placeholder
        // used to be after stripping all earlier ones
        slots.push(clean.len());
        prev_end = placeholder.start + placeholder.len;
    }
    clean.push_str(&content[prev_end..]);
    (clean, slots)
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
            description: None,
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
            description: Some(format!("description: {name}")),
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

    #[test]
    fn find_placeholders_locates_dollar_digit_and_at() {
        // `$1`, `$2`, ..., `$9`, `$@`, `$ARGUMENTS` are the slot syntax
        // shared with mush_ext::substitute_args. detection should return
        // them in source order with byte offsets so we can replace each
        // with an empty slot for interactive filling
        let content = "fix $1 in $2 and run $@";
        let found = find_placeholders(content);
        let positions: Vec<(usize, usize)> = found.iter().map(|p| (p.start, p.len)).collect();
        assert_eq!(
            positions,
            vec![(4, 2), (10, 2), (21, 2)],
            "expected three placeholders at $1, $2, $@"
        );
    }

    #[test]
    fn find_placeholders_recognises_arguments_long_form() {
        let found = find_placeholders("hello $ARGUMENTS world");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, 6);
        assert_eq!(found[0].len, 10);
    }

    #[test]
    fn find_placeholders_ignores_unrelated_dollar_signs() {
        // `$0`, `$foo`, `$$`, and a bare trailing `$` are not slots
        let found = find_placeholders("price $0 and $foo, $$ and $");
        assert!(found.is_empty(), "expected no slots, got {found:?}");
    }

    #[test]
    fn strip_placeholders_returns_cleaned_text_and_slot_offsets() {
        // input: "fix $1 in $2 file"
        //         0123456789012345678 (positions)
        // after stripping the two `$N`s (each 2 bytes), slot offsets in
        // the cleaned text are where each placeholder used to be:
        //   $1 was at byte 4 → slot 0 lives at byte 4 of the clean text
        //   $2 was at byte 10 → after removing $1 (-2), it lives at 8
        let (clean, slots) = strip_placeholders("fix $1 in $2 file");
        assert_eq!(clean, "fix  in  file");
        assert_eq!(slots, vec![4, 8]);
    }

    #[test]
    fn strip_placeholders_returns_empty_slots_when_none_present() {
        let (clean, slots) = strip_placeholders("plain text");
        assert_eq!(clean, "plain text");
        assert!(slots.is_empty());
    }
}
