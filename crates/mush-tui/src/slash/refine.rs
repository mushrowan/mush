use mush_session::harness::RefinementResult;

#[must_use]
pub fn format_refinement(result: &RefinementResult) -> String {
    let heading = match &result.rollback_of {
        Some(target) => format!(
            "━ harness rollback of {target} ({}) ━",
            result.scope.as_str()
        ),
        None => format!(
            "━ harness refined [{}] ({}) ━",
            result.id,
            result.scope.as_str()
        ),
    };
    let applied = result
        .applied_edits
        .iter()
        .filter(|edit| edit.applied)
        .count();
    let mut text = format!("{heading}\n\n{}", result.notice());
    if applied == 0 && result.applied_edits.is_empty() {
        text.push_str("\n\nno edits proposed");
        if !result.rationale.is_empty() {
            text.push_str(&format!(": {}", result.rationale));
        }
    }
    for edit in result.applied_edits.iter().filter(|edit| !edit.applied) {
        text.push_str(&format!(
            "\n- rejected {} {}:{}: {}",
            edit.edit.action.as_str(),
            edit.edit.kind.as_str(),
            edit.id,
            edit.error.as_deref().unwrap_or("unknown reason")
        ));
    }
    if !result.expected_outcome.is_empty() {
        text.push_str(&format!("\n\nexpected: {}", result.expected_outcome));
    }
    text.push_str(&format!("\n\n/refine rollback {} undoes this", result.id));
    text
}

#[must_use]
pub fn format_harness_digest(digest: &str) -> String {
    format!("━ harness digest ━\n\n{digest}")
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
        assert!(
            text.starts_with("━ harness refined [ref_abc] (local) ━"),
            "{text}"
        );
        assert!(
            text.contains("- create memory [local:test_runner] test runner: use cargo nextest"),
            "{text}"
        );
        assert!(
            text.contains("- rejected delete memory:missing: entry not found"),
            "{text}"
        );
        assert!(text.contains("expected: fewer cargo test runs"), "{text}");
        assert!(
            text.ends_with("/refine rollback ref_abc undoes this"),
            "{text}"
        );
    }
}
