use std::sync::{Arc, Mutex, PoisonError};

#[derive(Debug, Clone, Default)]
pub struct ModelSelection(Arc<Mutex<Selection>>);

#[derive(Debug, Default)]
struct Selection {
    current: String,
    subagent: Option<String>,
}

impl ModelSelection {
    #[must_use]
    pub fn new(current: String, subagent: Option<String>) -> Self {
        Self(Arc::new(Mutex::new(Selection { current, subagent })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Selection> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_current(&self, id: String) {
        self.lock().current = id;
    }

    #[must_use]
    pub fn current(&self) -> String {
        self.lock().current.clone()
    }

    pub fn set_subagent(&self, id: Option<String>) {
        self.lock().subagent = id;
    }

    #[must_use]
    pub fn subagent(&self) -> Option<String> {
        self.lock().subagent.clone()
    }

    #[must_use]
    pub fn for_child(&self, requested: Option<&str>) -> Option<String> {
        let selection = self.lock();
        requested
            .map(str::to_string)
            .or_else(|| selection.subagent.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_current_model_follows_the_session() {
        let models = ModelSelection::new("one".into(), None);
        assert_eq!(models.current(), "one");
        models.set_current("two".into());
        assert_eq!(models.current(), "two");
    }
}
