use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc;

use crate::fuzzy::FuzzyFilter;
use mush_session::SessionMeta;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    ThisDir,
    AllDirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Working,
    Idle,
    Paused,
    Stale,
    Failed,
    Elsewhere,
}

impl Liveness {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Stale => "stale",
            Self::Failed => "failed",
            Self::Elsewhere => "elsewhere",
        }
    }
}

pub type Roster = Vec<(mush_ai::types::SessionId, Liveness)>;

#[derive(Debug)]
pub struct SessionPickerState {
    pub sessions: Vec<SessionMeta>,
    pub selected: usize,
    pub filter: String,
    pub scope: SessionScope,
    pub cwd: String,
    pub loading: bool,
    pub confirm_delete: Option<SessionMeta>,
    pub toast: Option<String>,
    pub live: HashMap<mush_ai::types::SessionId, Liveness>,
    incoming: Option<mpsc::Receiver<SessionMeta>>,
    roster: Option<mpsc::Receiver<Roster>>,
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
            confirm_delete: None,
            toast: None,
            live: HashMap::new(),
            incoming: None,
            roster: None,
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    #[must_use]
    pub fn new_streaming(incoming: mpsc::Receiver<SessionMeta>, cwd: String) -> Self {
        Self {
            loading: true,
            incoming: Some(incoming),
            ..Self::new(Vec::new(), cwd)
        }
    }

    pub fn arm_delete(&mut self) -> bool {
        let selected = filtered_sessions(self)
            .get(self.selected)
            .map(|meta| (*meta).clone());
        self.confirm_delete = selected;
        self.confirm_delete.is_some()
    }

    pub fn cancel_delete(&mut self) {
        self.confirm_delete = None;
    }

    pub fn remove(&mut self, id: &mush_ai::types::SessionId) {
        self.sessions.retain(|meta| &meta.id != id);
        let visible = filtered_sessions(self).len();
        self.selected = self.selected.min(visible.saturating_sub(1));
    }

    pub fn watch_roster(&mut self, roster: mpsc::Receiver<Roster>) {
        self.roster = Some(roster);
    }

    fn drain_roster(&mut self) -> bool {
        let Some(rx) = &self.roster else {
            return false;
        };
        match rx.try_recv() {
            Ok(roster) => {
                self.live = roster.into_iter().collect();
                self.roster = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.roster = None;
                false
            }
        }
    }

    pub fn drain_incoming(&mut self) -> bool {
        let roster_changed = self.drain_roster();
        let Some(rx) = &self.incoming else {
            return roster_changed;
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
            added |= self.loading;
            self.loading = false;
            self.incoming = None;
        }
        added || roster_changed
    }
}

#[must_use]
pub fn listed(meta: &SessionMeta) -> bool {
    meta.parent.is_none() && meta.lifecycle.is_live()
}

impl crate::filtered_list::FilteredList for SessionPickerState {
    fn filter_and_selected(&mut self) -> (&mut String, &mut usize) {
        (&mut self.filter, &mut self.selected)
    }

    fn visible_len(&self) -> usize {
        filtered_sessions(self).len()
    }
}

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
