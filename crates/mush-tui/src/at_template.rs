use mush_ext::PromptTemplate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtTrigger {
    pub start: usize,
    pub word: String,
}

#[must_use]
pub fn parse_at_trigger(text: &str, cursor: usize) -> Option<AtTrigger> {
    if cursor > text.len() {
        return None;
    }
    let before = &text[..cursor];

    let word_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_at_word_char(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(cursor);

    if word_start == 0 {
        return None;
    }
    let at_pos = before[..word_start]
        .char_indices()
        .next_back()
        .filter(|(_, c)| *c == '@')
        .map(|(i, _)| i)?;

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

#[must_use]
pub fn find_exact<'a>(
    templates: &'a [PromptTemplate],
    trigger: &AtTrigger,
) -> Option<&'a PromptTemplate> {
    templates.iter().find(|t| t.name == trigger.word)
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    pub start: usize,
    pub len: usize,
}

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
        assert!(parse_at_trigger("send to me@example", 18).is_none());
    }

    #[test]
    fn parse_at_word_with_cursor_mid_word_still_returns_word_so_far() {
        let trigger = parse_at_trigger("@review", 4).expect("mid word");
        assert_eq!(trigger.start, 0);
        assert_eq!(trigger.word, "rev");
    }

    #[test]
    fn parse_at_followed_by_non_word_stops_the_trigger() {
        assert!(parse_at_trigger("@review-foo", 11).is_none());
    }

    #[test]
    fn parse_at_in_middle_of_alphanumeric_text_is_not_a_trigger() {
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
        let found = find_placeholders("price $0 and $foo, $$ and $");
        assert!(found.is_empty(), "expected no slots, got {found:?}");
    }

    #[test]
    fn strip_placeholders_returns_cleaned_text_and_slot_offsets() {
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
