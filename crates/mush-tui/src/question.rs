use crossterm::event::KeyCode;

use crate::app::{App, AppMode};
use crate::app_state::KernelBusy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Question {
    Tool,
    KernelBusy,
    BranchSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    pub keys: &'static str,
    pub label: &'static str,
}

impl Question {
    #[must_use]
    pub fn open(app: &App) -> Option<Self> {
        if app.interaction.mode != AppMode::ToolConfirm {
            return None;
        }
        Some(if app.interaction.kernel_busy == KernelBusy::Asked {
            Self::KernelBusy
        } else if app.interaction.branch_summary_ask.is_some() {
            Self::BranchSummary
        } else {
            Self::Tool
        })
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Tool => " allow this tool call? ",
            Self::KernelBusy => " python kernel busy ",
            Self::BranchSummary => " leaving this branch ",
        }
    }

    #[must_use]
    pub fn choices(self) -> &'static [Choice] {
        match self {
            Self::Tool => &[
                Choice {
                    keys: "y/enter",
                    label: "allow",
                },
                Choice {
                    keys: "n/esc",
                    label: "deny",
                },
            ],
            Self::KernelBusy => &[
                Choice {
                    keys: "y/enter",
                    label: "wait",
                },
                Choice {
                    keys: "n",
                    label: "kill and restart",
                },
                Choice {
                    keys: "esc",
                    label: "cancel",
                },
            ],
            Self::BranchSummary => &[
                Choice {
                    keys: "y/enter",
                    label: "summarise",
                },
                Choice {
                    keys: "n",
                    label: "just jump",
                },
                Choice {
                    keys: "esc",
                    label: "stay",
                },
            ],
        }
    }
}

impl Answer {
    #[must_use]
    pub fn from_key(code: KeyCode) -> Option<Self> {
        match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(Self::Yes),
            KeyCode::Char('n' | 'N') => Some(Self::No),
            KeyCode::Esc => Some(Self::Dismiss),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;

    fn app() -> App {
        App::new("test".into(), TokenCount::new(200_000))
    }

    #[test]
    fn nothing_is_asked_outside_the_confirm_mode() {
        let mut app = app();
        app.interaction.confirm_prompt = Some("bash ls".into());
        app.interaction.kernel_busy = KernelBusy::Asked;
        assert_eq!(Question::open(&app), None);
    }

    #[test]
    fn the_kernel_question_wins_over_the_branch_and_tool_questions() {
        let mut app = app();
        app.interaction.mode = AppMode::ToolConfirm;
        assert_eq!(Question::open(&app), Some(Question::Tool));

        app.interaction.branch_summary_ask = Some(mush_session::tree::EntryId::from("e1"));
        assert_eq!(Question::open(&app), Some(Question::BranchSummary));

        app.interaction.kernel_busy = KernelBusy::Asked;
        assert_eq!(Question::open(&app), Some(Question::KernelBusy));
    }

    #[test]
    fn keys_map_to_answers() {
        for code in [KeyCode::Char('y'), KeyCode::Char('Y'), KeyCode::Enter] {
            assert_eq!(Answer::from_key(code), Some(Answer::Yes));
        }
        for code in [KeyCode::Char('n'), KeyCode::Char('N')] {
            assert_eq!(Answer::from_key(code), Some(Answer::No));
        }
        assert_eq!(Answer::from_key(KeyCode::Esc), Some(Answer::Dismiss));
        for code in [KeyCode::Char('x'), KeyCode::Tab, KeyCode::Backspace] {
            assert_eq!(Answer::from_key(code), None);
        }
    }

    #[test]
    fn every_question_offers_yes_first() {
        for question in [
            Question::Tool,
            Question::KernelBusy,
            Question::BranchSummary,
        ] {
            assert!(question.choices()[0].keys.starts_with('y'));
            assert!(question.choices().iter().any(|c| c.keys.contains("esc")));
        }
    }
}
