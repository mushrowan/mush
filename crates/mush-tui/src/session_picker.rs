//! session picker state and filtering helpers

use std::cell::RefCell;
use std::sync::mpsc;

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
    /// still receiving sessions from the background loader. UI renders
    /// a "loading…" hint while this is true so the user knows more are
    /// coming
    pub loading: bool,
    /// background loader channel. sessions arrive asynchronously and
    /// the main event loop drains them on every frame tick. `None` for
    /// pickers opened with a pre-built session list (e.g. tests)
    incoming: Option<mpsc::Receiver<SessionMeta>>,
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
            loading: false,
            incoming: None,
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    /// open the picker with a background loader feeding sessions via
    /// `incoming`. starts empty and `loading = true`; the caller drives
    /// ingestion by calling [`SessionPickerState::drain_incoming`] every
    /// frame tick
    #[must_use]
    pub fn new_streaming(incoming: mpsc::Receiver<SessionMeta>, cwd: String) -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            filter: String::new(),
            scope: SessionScope::ThisDir,
            cwd,
            loading: true,
            incoming: Some(incoming),
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    /// drain any sessions already produced by the background loader
    /// into the `sessions` vec, keeping `updated_at desc` ordering.
    /// flips `loading` to `false` once the sender disconnects.
    /// returns `true` when new sessions arrived this call, so callers
    /// can request a redraw without polling every frame unconditionally
    pub fn drain_incoming(&mut self) -> bool {
        let Some(rx) = &self.incoming else {
            return false;
        };
        let mut added = false;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(meta) => {
                    self.sessions.push(meta);
                    added = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if added {
            self.sessions
                .sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        }
        if disconnected {
            self.loading = false;
            self.incoming = None;
        }
        added
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
