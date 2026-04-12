//! session picker state and filtering helpers

use mush_session::SessionMeta;

/// session picker scope
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    /// sessions from current working directory only
    ThisDir,
    /// all sessions across all directories
    AllDirs,
}

/// state for the session picker overlay
#[derive(Debug, Clone)]
pub struct SessionPickerState {
    pub sessions: Vec<SessionMeta>,
    pub selected: usize,
    pub filter: String,
    pub scope: SessionScope,
    /// current working directory for scope filtering
    pub cwd: String,
}

impl SessionPickerState {
    #[must_use]
    pub fn new(sessions: Vec<SessionMeta>, cwd: String) -> Self {
        Self {
            sessions,
            selected: 0,
            filter: String::new(),
            scope: SessionScope::ThisDir,
            cwd,
        }
    }
}

/// get sessions matching the current filter and scope
#[must_use]
pub fn filtered_sessions(picker: &SessionPickerState) -> Vec<&SessionMeta> {
    let scope_filtered: Vec<&SessionMeta> = match picker.scope {
        SessionScope::ThisDir => picker
            .sessions
            .iter()
            .filter(|s| s.cwd == picker.cwd)
            .collect(),
        SessionScope::AllDirs => picker.sessions.iter().collect(),
    };

    if picker.filter.is_empty() {
        scope_filtered
    } else {
        let filter_lower = picker.filter.to_lowercase();
        scope_filtered
            .into_iter()
            .filter(|s| {
                s.title
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&filter_lower)
                    || s.id.contains(&filter_lower)
                    || s.cwd.to_lowercase().contains(&filter_lower)
            })
            .collect()
    }
}
