//! session picker state and filtering helpers

use std::cell::RefCell;

use crate::fuzzy::FuzzyFilter;
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
#[derive(Debug)]
pub struct SessionPickerState {
    pub sessions: Vec<SessionMeta>,
    pub selected: usize,
    pub filter: String,
    pub scope: SessionScope,
    /// current working directory for scope filtering
    pub cwd: String,
    /// cached fuzzy matcher reused across filter calls. `RefCell` since
    /// `filtered_sessions` takes `&self` for read-only render paths but
    /// nucleo's scoring wants `&mut Matcher`
    matcher: RefCell<FuzzyFilter>,
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
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }
}

/// get sessions matching the current filter and scope. empty filter returns
/// all scope-matched sessions in original order, otherwise fuzzy-ranks by
/// title + id + cwd with highest score first
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
        return scope_filtered;
    }

    // score against the concatenation of searchable fields so a match in
    // any of title/id/cwd contributes to the ranking
    let haystacks: Vec<String> = scope_filtered
        .iter()
        .map(|s| {
            let title = s.title.as_deref().unwrap_or("");
            format!("{title} {} {}", s.id.as_str(), s.cwd)
        })
        .collect();
    let mut matcher = picker.matcher.borrow_mut();
    let indices = matcher.filter(&haystacks, &picker.filter, String::as_str);
    indices.into_iter().map(|i| scope_filtered[i]).collect()
}
