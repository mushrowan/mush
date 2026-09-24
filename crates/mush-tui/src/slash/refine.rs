use mush_session::harness::{Kind, RefinementResult};

fn plural(kind: Kind, count: usize) -> String {
    match (kind, count) {
        (Kind::Memory, 1) => "memory".into(),
        (Kind::Memory, _) => "memories".into(),
        (kind, 1) => kind.as_str().into(),
        (kind, _) => format!("{}s", kind.as_str()),
    }
}

fn headline(result: &RefinementResult) -> String {
    let rollback = result.rollback_of.is_some();
    let total = result.applied_edits.len();
    let applied: Vec<_> = result
        .applied_edits
        .iter()
        .filter(|edit| edit.applied)
        .collect();
    if total == 0 {
        return if rollback {
            "harness rollback unchanged, no edits applied".into()
        } else {
            "harness unchanged, no edits applied".into()
        };
    }
    if applied.is_empty() {
        let what = if rollback { "rollback" } else { "refinement" };
        return format!("harness {what} failed, 0 of {total} edits applied");
    }
    if applied.len() < total {
        let what = if rollback {
            "partially rolled back"
        } else {
            "partially refined"
        };
        return format!("harness {what}, {} of {total} edits applied", applied.len());
    }
    if rollback {
        return format!("harness rolled back, {total} edits applied");
    }
    let first = applied[0];
    if applied.iter().all(|edit| edit.edit.kind == first.edit.kind) {
        let verb = if applied
            .iter()
            .all(|edit| edit.edit.action == first.edit.action)
        {
            first.edit.action.past_tense()
        } else {
            "changed"
        };
        return format!(
            "harness refined, {} {} {verb}",
            applied.len(),
            plural(first.edit.kind, applied.len())
        );
    }
    format!("harness refined, {} edits applied", applied.len())
}

#[must_use]
pub fn format_refinement(result: &RefinementResult) -> String {
    let mut text = format!("◆ {}", headline(result));
    let summary = result.summary.trim();
    if summary.is_empty() {
        if !result.rationale.trim().is_empty() {
            text.push_str(&format!("\n{}", result.rationale.trim()));
        }
    } else {
        text.push_str(&format!("\n{summary}"));
    }
    for edit in result.applied_edits.iter().filter(|edit| !edit.applied) {
        text.push_str(&format!(
            "\nrejected {} {}:{}: {}",
            edit.edit.action.as_str(),
            edit.edit.kind.as_str(),
            edit.id,
            edit.error.as_deref().unwrap_or("unknown reason")
        ));
    }
    if result.applied_edits.iter().any(|edit| edit.applied) {
        text.push_str(&format!("\n/refine rollback {} undoes this", result.id));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_session::harness::{Action, Edit, Kind, Proposal, Scope, State};

    #[test]
    fn a_refinement_notice_lists_edits_and_the_rollback_hint() {
        let mut state = State::default();
        let result = state.apply(
            &Proposal {
                summary: "remember the test runner".into(),
                rationale: "nextest came up three times".into(),
                expected_outcome: "fewer cargo test runs".into(),
                edits: vec![
                    Edit {
                        action: Action::Create,
                        kind: Kind::Memory,
                        id: None,
                        title: Some("test runner".into()),
                        content: Some("use cargo nextest".into()),
                        path: None,
                        reference: None,
                        arguments: None,
                        metadata: None,
                        reason: None,
                    },
                    Edit {
                        action: Action::Delete,
                        kind: Kind::Memory,
                        id: Some("missing".into()),
                        title: None,
                        content: None,
                        path: None,
                        reference: None,
                        arguments: None,
                        metadata: None,
                        reason: None,
                    },
                ],
            },
            "ref_abc",
            Scope::Local,
            None,
        );
        let text = format_refinement(&result);
        assert_eq!(
            text,
            "◆ harness partially refined, 1 of 2 edits applied\n\
             remember the test runner\n\
             rejected delete memory:missing: entry not found\n\
             /refine rollback ref_abc undoes this"
        );
        assert!(!text.contains("use cargo nextest"), "{text}");
        assert!(!text.contains("fewer cargo test runs"), "{text}");
    }

    fn applied(action: Action, kind: Kind, id: &str) -> mush_session::harness::AppliedEdit {
        mush_session::harness::AppliedEdit {
            edit: Edit {
                action,
                kind,
                id: Some(id.into()),
                title: Some(id.into()),
                content: Some("body".into()),
                path: None,
                reference: None,
                arguments: None,
                metadata: None,
                reason: None,
            },
            id: id.into(),
            applied: true,
            error: None,
            before: None,
            after: None,
        }
    }

    fn result_of(edits: Vec<mush_session::harness::AppliedEdit>) -> RefinementResult {
        RefinementResult {
            id: "ref_abc".into(),
            summary: "six memories".into(),
            rationale: String::new(),
            expected_outcome: "less re-deriving".into(),
            applied_edits: edits,
            scope: Scope::Local,
            rollback_of: None,
        }
    }

    #[test]
    fn a_batch_of_one_kind_is_counted_not_listed() {
        let edits = (0..6)
            .map(|i| applied(Action::Create, Kind::Memory, &format!("m{i}")))
            .collect();
        let text = format_refinement(&result_of(edits));

        assert_eq!(
            text,
            "◆ harness refined, 6 memories created\n\
             six memories\n\
             /refine rollback ref_abc undoes this"
        );
    }

    #[test]
    fn mixed_kinds_and_actions_fall_back_to_a_count() {
        let mixed = vec![
            applied(Action::Create, Kind::Memory, "m"),
            applied(Action::Update, Kind::Skill, "s"),
        ];
        assert!(
            format_refinement(&result_of(mixed)).starts_with("◆ harness refined, 2 edits applied")
        );

        let one_kind = vec![
            applied(Action::Create, Kind::Prompt, "a"),
            applied(Action::Delete, Kind::Prompt, "b"),
        ];
        assert!(
            format_refinement(&result_of(one_kind))
                .starts_with("◆ harness refined, 2 prompts changed")
        );
    }

    #[test]
    fn nothing_proposed_says_so_and_offers_no_rollback() {
        let text = format_refinement(&result_of(Vec::new()));

        assert_eq!(text, "◆ harness unchanged, no edits applied\nsix memories");
    }
}
