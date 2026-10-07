use std::sync::Arc;
use std::sync::nonpoison::Mutex;

#[derive(Debug, Clone, Default)]
pub struct ModelSelection(Arc<Mutex<Selection>>);

#[derive(Debug, Default)]
struct Selection {
    current: String,
    subagent: Option<String>,
    thinking: Option<mush_ai::types::ThinkingLevel>,
    accounts: mush_ai::types::SessionAccounts,
}

impl ModelSelection {
    #[must_use]
    pub fn new(current: String, subagent: Option<String>) -> Self {
        Self(Arc::new(Mutex::new(Selection {
            current,
            subagent,
            thinking: None,
            accounts: mush_ai::types::SessionAccounts::default(),
        })))
    }

    pub fn set_current(&self, id: String) {
        self.0.lock().current = id;
    }

    #[must_use]
    pub fn current(&self) -> String {
        self.0.lock().current.clone()
    }

    pub fn set_thinking(&self, level: mush_ai::types::ThinkingLevel) {
        self.0.lock().thinking = Some(level);
    }

    #[must_use]
    pub fn thinking(&self) -> Option<mush_ai::types::ThinkingLevel> {
        self.0.lock().thinking.clone()
    }

    pub fn set_subagent(&self, id: Option<String>) {
        self.0.lock().subagent = id;
    }

    #[must_use]
    pub fn subagent(&self) -> Option<String> {
        self.0.lock().subagent.clone()
    }

    pub fn set_accounts(&self, accounts: mush_ai::types::SessionAccounts) {
        self.0.lock().accounts = accounts;
    }

    #[must_use]
    pub fn accounts(&self) -> mush_ai::types::SessionAccounts {
        self.0.lock().accounts.clone()
    }

    #[must_use]
    pub fn for_child(&self, requested: Option<&str>) -> Option<String> {
        let selection = self.0.lock();
        requested
            .map(str::to_string)
            .or_else(|| selection.subagent.clone())
    }
}

pub fn child_thinking(
    model: &mush_ai::types::Model,
    requested: Option<&mush_ai::types::ThinkingLevel>,
    parent: &mush_ai::types::ThinkingLevel,
) -> Result<mush_ai::types::ThinkingLevel, String> {
    use mush_ai::types::ThinkingLevel;
    let supported = &model.supported_thinking_levels;
    match requested {
        Some(level) => {
            if *level == ThinkingLevel::Off || supported.is_empty() || supported.contains(level) {
                return Ok(level.clone());
            }
            let names: Vec<&str> = std::iter::once("off")
                .chain(supported.iter().filter_map(ThinkingLevel::effort_name))
                .collect();
            Err(format!(
                "thinking level {} is not supported by {}, supported levels: {}",
                level.effort_name().unwrap_or("off"),
                model.id,
                names.join(", ")
            ))
        }
        None if *parent == ThinkingLevel::Off => Ok(ThinkingLevel::Off),
        None => Ok(model.clamp_thinking(parent)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_follows_the_parent_thinking_clamped_to_its_model() {
        use mush_ai::types::ThinkingLevel;
        let mut model = crate::testing::echo_model();
        model.supported_thinking_levels = vec![ThinkingLevel::Low, ThinkingLevel::Medium];
        assert_eq!(
            child_thinking(&model, None, &ThinkingLevel::Xhigh),
            Ok(ThinkingLevel::Medium)
        );
        assert_eq!(
            child_thinking(&model, None, &ThinkingLevel::Off),
            Ok(ThinkingLevel::Off)
        );
        assert_eq!(
            child_thinking(&model, Some(&ThinkingLevel::Low), &ThinkingLevel::High),
            Ok(ThinkingLevel::Low)
        );
        let refused =
            child_thinking(&model, Some(&ThinkingLevel::High), &ThinkingLevel::Off).unwrap_err();
        assert!(refused.contains("off, low, medium"), "{refused}");
    }

    #[test]
    fn a_child_takes_what_it_asked_for_then_the_session_override() {
        let models = ModelSelection::new("parent-model".into(), None);
        assert_eq!(models.for_child(Some("asked")).as_deref(), Some("asked"));
        assert!(models.for_child(None).is_none());

        models.set_subagent(Some("cheap".into()));
        assert_eq!(models.for_child(None).as_deref(), Some("cheap"));
        assert_eq!(models.for_child(Some("asked")).as_deref(), Some("asked"));

        models.set_subagent(None);
        assert!(models.for_child(None).is_none());
    }

    #[test]
    fn a_child_sees_the_accounts_the_session_holds() {
        let models = ModelSelection::new("one".into(), None);
        assert!(models.accounts().is_empty());
        let mut accounts = mush_ai::types::SessionAccounts::default();
        accounts.set(
            "anthropic",
            Some(mush_ai::types::AccountLabel::parse("work").unwrap()),
        );
        models.set_accounts(accounts.clone());
        assert_eq!(models.accounts(), accounts);
    }

    #[test]
    fn the_current_model_follows_the_session() {
        let models = ModelSelection::new("one".into(), None);
        assert_eq!(models.current(), "one");
        models.set_current("two".into());
        assert_eq!(models.current(), "two");
        assert!(models.thinking().is_none());
        models.set_thinking(mush_ai::types::ThinkingLevel::High);
        assert_eq!(models.thinking(), Some(mush_ai::types::ThinkingLevel::High));
    }
}
