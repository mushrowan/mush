use std::path::Path;

use chrono::{DateTime, Utc};

const WORK_GUIDANCE: &str = "continue authorised work within the task scope. keep a record of decisions, checks and unresolved issues. stop and report blocked when the next step needs approval or essential information.";

pub fn assignment(base: &str, round: u32, report: &Path, deadline: DateTime<Utc>) -> String {
    let mut prompt = format!("{base}\n\n{WORK_GUIDANCE}");
    if round > 1 {
        prompt.push_str(&format!(
            "\n\nautomatic continuation {round}. read {} and inspect jj status before working. continue the remaining authorised task. don't repeat finished work. preserve blocked/done states. deadline {deadline}. external effects remain forbidden.",
            report.display(),
        ));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_assignment_and_continuations_keep_work_boundaries() {
        let deadline = DateTime::from_timestamp(1_789_200_000, 0).unwrap();
        for round in [1, 2, 20] {
            let prompt = assignment("fixture task", round, Path::new("state.json"), deadline);
            assert!(prompt.starts_with("fixture task\n\n"));
            assert!(prompt.contains(WORK_GUIDANCE));
            assert_eq!(prompt.matches(WORK_GUIDANCE).count(), 1);
            assert_eq!(prompt.contains("automatic continuation"), round > 1);
            if round > 1 {
                assert!(prompt.contains("preserve blocked/done states"));
                assert!(prompt.contains("external effects remain forbidden"));
                assert!(prompt.contains("state.json"));
                assert!(prompt.contains(&deadline.to_string()));
            }
            for claim in [
                "unattended",
                "unmonitored",
                "unsupervised",
                "user is away",
                "user is absent",
                "nobody is watching",
                "you are being monitored",
                "user is watching",
            ] {
                assert!(!prompt.contains(claim), "{claim}");
            }
        }
    }

    #[test]
    fn task_text_is_preserved() {
        let deadline = DateTime::from_timestamp(1_789_200_000, 0).unwrap();
        let base = "quoted input: user is away\nkeep this text unchanged";
        assert!(assignment(base, 1, Path::new("state.json"), deadline).starts_with(base));
    }
}
