use mush_ai::types::{Message, Timestamp, UserContent, UserMessage};

pub const RESTORED_TAG: &str = "[python-state-restored]";
pub const PERSISTED_TAG: &str = "[python-state]";

#[must_use]
pub fn restored(outcome: &Result<mush_kernel::RestoreReport, String>) -> Option<Message> {
    let mut lines = vec![RESTORED_TAG.to_string(), String::new()];
    let failed = match outcome {
        Ok(report) if report.restored.is_empty() && report.failed.is_empty() => return None,
        Ok(report) => &report.failed[..],
        Err(_) => &[],
    };
    match outcome {
        Ok(report) if !report.restored.is_empty() => lines.push(format!(
            "Your Python kernel state was revived from your previous session. These names are \
             available again: {}.",
            report.restored.join(", ")
        )),
        Ok(_) | Err(_) => lines.push(
            "Your previous Python kernel state could not be revived; the kernel is starting \
             fresh, so re-create any variables, imports, or loaded data you need."
                .to_string(),
        ),
    }
    if !failed.is_empty() {
        lines.push(format!(
            "These could not be restored and must be recreated if needed: {}.",
            failed.join(", ")
        ));
    }
    Some(notice(lines.join("\n")))
}

#[must_use]
pub fn persisted(names: &Result<Vec<String>, String>) -> Message {
    let detail = match names {
        Ok(names) if names.is_empty() => " You have not defined any names yet.".to_string(),
        Ok(names) => format!(" These names are still defined: {}.", names.join(", ")),
        Err(_) => String::new(),
    };
    notice(format!(
        "{PERSISTED_TAG}\n\nYour Python kernel persisted through compaction; its remaining \
         variables, imports, and helpers are still available.{detail}"
    ))
}

pub async fn after_compaction(kernel: Option<&dyn mush_kernel::KernelState>) -> Option<Message> {
    let names = kernel?.live_names().await?;
    Some(persisted(&names))
}

#[must_use]
pub fn is_notice(text: &str) -> bool {
    [RESTORED_TAG, PERSISTED_TAG].iter().any(|tag| {
        text.strip_prefix(tag)
            .is_some_and(|rest| rest.starts_with('\n'))
    })
}

fn notice(text: String) -> Message {
    Message::User(
        UserMessage::builder()
            .content(UserContent::Text(text))
            .timestamp_ms(Timestamp::now())
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(message: &Message) -> String {
        let Message::User(user) = message else {
            panic!("{message:?}")
        };
        user.text()
    }

    fn report(restored: &[&str], failed: &[&str]) -> mush_kernel::RestoreReport {
        mush_kernel::RestoreReport {
            restored: restored.iter().map(ToString::to_string).collect(),
            failed: failed.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn a_revived_state_names_what_came_back_and_what_did_not() {
        let message = restored(&Ok(report(&["a", "b"], &["sock"]))).unwrap();

        assert_eq!(
            text(&message),
            "[python-state-restored]\n\nYour Python kernel state was revived from your previous \
             session. These names are available again: a, b.\nThese could not be restored and \
             must be recreated if needed: sock."
        );
        assert!(is_notice(&text(&message)));
        assert!(!is_notice("[python-state-restoredx]\n"));
        assert!(!is_notice("hello"));
    }

    #[test]
    fn a_failed_restore_says_the_kernel_starts_fresh() {
        let failed = restored(&Err("kernel exited".into())).unwrap();
        let nothing_back = restored(&Ok(report(&[], &["sock"]))).unwrap();

        assert_eq!(
            text(&failed),
            "[python-state-restored]\n\nYour previous Python kernel state could not be revived; \
             the kernel is starting fresh, so re-create any variables, imports, or loaded data \
             you need."
        );
        assert!(text(&nothing_back).starts_with(&text(&failed)));
        assert!(text(&nothing_back).ends_with("recreated if needed: sock."));
    }

    #[test]
    fn an_empty_snapshot_needs_no_notice() {
        assert_eq!(restored(&Ok(report(&[], &[]))), None);
    }

    #[test]
    fn the_compaction_notice_lists_the_live_names() {
        let listed = persisted(&Ok(vec!["df".into(), "helper".into()]));
        let empty = persisted(&Ok(Vec::new()));
        let unknown = persisted(&Err("timed out".into()));

        assert_eq!(
            text(&listed),
            "[python-state]\n\nYour Python kernel persisted through compaction; its remaining \
             variables, imports, and helpers are still available. These names are still \
             defined: df, helper."
        );
        assert!(text(&empty).ends_with(" You have not defined any names yet."));
        assert!(text(&unknown).ends_with("helpers are still available."));
        assert!(is_notice(&text(&listed)));
        assert!(!is_notice("[python-state-restoredx]\n"));
        assert!(!is_notice("hello"));
    }
}
