use std::cell::Cell;
use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mush_runtime::{Answer, ChildSummary, Decision, DecisionKey, DecisionStatus, Draft};

use crate::app::{App, AppEvent, AppMode};
use crate::keybinds::Action;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKey {
    Decision(DecisionKey),
    Confirm(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorKind {
    Own,
    Clarify,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub kind: EditorKind,
    pub text: String,
    pub cursor: usize,
}

impl Editor {
    fn new(kind: EditorKind, text: String) -> Self {
        let cursor = text.len();
        Self { kind, text, cursor }
    }

    pub fn insert(&mut self, text: &str) {
        let clean: String = text
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    fn backspace(&mut self) {
        if let Some((at, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.text.replace_range(at..self.cursor, "");
            self.cursor = at;
        }
    }

    fn left(&mut self) {
        if let Some((at, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = at;
        }
    }

    fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
}

#[derive(Debug, Default)]
pub struct DecisionsPanel {
    pub open: bool,
    pub selected: Option<ItemKey>,
    pub editor: Option<Editor>,
    pub review: bool,
    pub review_scroll: Cell<usize>,
    pub confirms: BTreeMap<String, bool>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum Item<'a> {
    Decision(&'a Decision),
    Confirm(&'a ChildSummary),
}

impl Item<'_> {
    #[must_use]
    pub fn key(&self) -> ItemKey {
        match self {
            Self::Decision(decision) => ItemKey::Decision(decision.key.clone()),
            Self::Confirm(child) => ItemKey::Confirm(child.id.clone()),
        }
    }

    #[must_use]
    pub fn blocking(&self) -> bool {
        match self {
            Self::Decision(decision) => decision.blocking && decision.waiting_on_user(),
            Self::Confirm(_) => true,
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Decision(decision) => &decision.key.id,
            Self::Confirm(_) => "tool",
        }
    }

    #[must_use]
    pub fn question(&self) -> String {
        match self {
            Self::Decision(decision) => decision.question.clone(),
            Self::Confirm(child) => format!(
                "allow this tool call? {}",
                child.awaiting_confirmation.as_deref().unwrap_or_default()
            ),
        }
    }

    #[must_use]
    pub fn options(&self) -> Vec<String> {
        match self {
            Self::Decision(decision) => decision
                .options
                .iter()
                .map(|option| option.label.clone())
                .collect(),
            Self::Confirm(_) => vec!["allow".into(), "deny".into()],
        }
    }
}

#[derive(Debug)]
pub struct Group<'a> {
    pub session: String,
    pub label: String,
    pub root: bool,
    pub items: Vec<Item<'a>>,
}

#[must_use]
pub fn groups<'a>(decisions: &'a [Decision], children: &'a [ChildSummary]) -> Vec<Group<'a>> {
    let mut groups: Vec<Group<'a>> = Vec::new();
    let mut place = |session: &str, label: String, root: bool, item: Item<'a>| match groups
        .iter_mut()
        .find(|group| group.session == session)
    {
        Some(group) => group.items.push(item),
        None => groups.push(Group {
            session: session.to_owned(),
            label,
            root,
            items: vec![item],
        }),
    };
    for decision in decisions.iter().filter(|decision| !decision.is_closed()) {
        place(
            &decision.asker.session,
            decision.asker.label(),
            decision.asker.is_root(),
            Item::Decision(decision),
        );
    }
    for child in children
        .iter()
        .filter(|child| child.awaiting_confirmation.is_some())
    {
        place(
            &child.session_id,
            child.name.clone(),
            false,
            Item::Confirm(child),
        );
    }
    for group in &mut groups {
        group.items.sort_by_key(|item| !item.blocking());
    }
    groups.sort_by_key(|group| (!group.root, !group.items.iter().any(Item::blocking)));
    groups
}

#[must_use]
pub fn flat(groups: &[Group<'_>]) -> Vec<ItemKey> {
    groups
        .iter()
        .flat_map(|group| group.items.iter().map(Item::key))
        .collect()
}

#[must_use]
pub fn waiting(app: &App) -> (usize, usize) {
    let waiting = app
        .decisions
        .iter()
        .filter(|decision| decision.waiting_on_user());
    let open = waiting.clone().count();
    let blocking = waiting.filter(|decision| decision.blocking).count();
    (open, blocking)
}

#[must_use]
pub fn counts(app: &App) -> (usize, usize) {
    let (open, blocking) = waiting(app);
    let confirms = app
        .children
        .iter()
        .filter(|child| child.awaiting_confirmation.is_some())
        .count();
    (open + confirms, blocking + confirms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chip {
    Open,
    OpenText,
    Picked { letter: char, lean: bool },
    Own,
    Clarify,
    Later,
    Changed,
    Asked,
    Updated,
    OnLean,
    Allow(bool),
}

#[must_use]
pub fn chip(item: &Item<'_>, panel: &DecisionsPanel) -> Chip {
    match item {
        Item::Confirm(child) => match panel.confirms.get(&child.id) {
            Some(allow) => Chip::Allow(*allow),
            None => Chip::Open,
        },
        Item::Decision(decision) => {
            if matches!(decision.status, DecisionStatus::Clarifying { .. }) {
                return Chip::Asked;
            }
            match &decision.draft {
                Some(_) if decision.draft_is_stale() => Chip::Changed,
                Some(Draft {
                    answer: Answer::Pick { option, .. },
                    ..
                }) => Chip::Picked {
                    letter: mush_runtime::decisions::letter(*option),
                    lean: decision.lean == Some(*option),
                },
                Some(Draft {
                    answer: Answer::Own { .. },
                    ..
                }) => Chip::Own,
                Some(Draft {
                    answer: Answer::Clarify { .. },
                    ..
                }) => Chip::Clarify,
                Some(Draft {
                    answer: Answer::Later,
                    ..
                }) => Chip::Later,
                None if decision.clarified.is_some() => Chip::Updated,
                None if decision.past_assumption(mush_ai::types::Timestamp::now()) => Chip::OnLean,
                None if decision.options.is_empty() => Chip::OpenText,
                None => Chip::Open,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub keys: Vec<DecisionKey>,
    pub confirms: Vec<(String, bool)>,
}

impl Outgoing {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.confirms.is_empty()
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.keys.len() + self.confirms.len()
    }
}

#[must_use]
pub fn outgoing(app: &App) -> Outgoing {
    let keys = app
        .decisions
        .iter()
        .filter(|decision| !decision.is_closed())
        .filter(|decision| {
            decision
                .draft
                .as_ref()
                .is_some_and(|draft| draft.answer.sendable())
        })
        .map(|decision| decision.key.clone())
        .collect();
    let confirms = app
        .decisions_panel
        .confirms
        .iter()
        .filter(|(child, _)| {
            app.children
                .iter()
                .any(|known| &known.id == *child && known.awaiting_confirmation.is_some())
        })
        .map(|(child, allow)| (child.clone(), *allow))
        .collect();
    Outgoing { keys, confirms }
}

fn can_open(app: &App) -> bool {
    matches!(
        app.interaction.mode,
        AppMode::Insert | AppMode::Normal | AppMode::Visual
    )
}

pub fn open(app: &mut App) {
    app.decisions_panel.open = true;
    app.decisions_panel.review = false;
    app.decisions_panel.editor = None;
    app.decisions_panel.notice = None;
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<Option<AppEvent>> {
    let toggle = app.keymap.matches(Action::ToggleDecisions, key);
    if !app.decisions_panel.open {
        if toggle && can_open(app) {
            open(app);
            return Some(Some(AppEvent::DecisionsRefresh));
        }
        return None;
    }
    if app.decisions_panel.editor.is_some() {
        return Some(editor_key(app, key));
    }
    if app.decisions_panel.review {
        return Some(review_key(app, key, toggle));
    }
    Some(list_key(app, key, toggle))
}

pub fn paste(app: &mut App, text: &str) -> bool {
    if !app.decisions_panel.open {
        return false;
    }
    if let Some(editor) = app.decisions_panel.editor.as_mut() {
        editor.insert(text);
    }
    true
}

fn plain_char(key: KeyEvent) -> Option<char> {
    let extra = key.modifiers - KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Char(c) if extra.is_empty() => Some(c),
        _ => None,
    }
}

fn list_key(app: &mut App, key: KeyEvent, toggle: bool) -> Option<AppEvent> {
    if toggle || app.keymap.matches(Action::Cancel, key) {
        app.decisions_panel.open = false;
        app.decisions_panel.notice = None;
        return None;
    }
    app.decisions_panel.notice = None;
    let keys = flat(&groups(&app.decisions, &app.children));
    if keys.is_empty() {
        return None;
    }
    let position = app
        .decisions_panel
        .selected
        .as_ref()
        .and_then(|selected| keys.iter().position(|key| key == selected))
        .unwrap_or(0);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let step = |by: isize| {
        let last = keys.len() - 1;
        position.saturating_add_signed(by).min(last)
    };
    let moved = match key.code {
        KeyCode::Down => Some(step(1)),
        KeyCode::Up => Some(step(-1)),
        KeyCode::Char('n') if control => Some(step(1)),
        KeyCode::Char('p') if control => Some(step(-1)),
        KeyCode::Home => Some(0),
        KeyCode::End => Some(keys.len() - 1),
        KeyCode::PageDown => Some(step(10)),
        KeyCode::PageUp => Some(step(-10)),
        KeyCode::Tab => Some(group_step(app, &keys, position, true)),
        KeyCode::BackTab => Some(group_step(app, &keys, position, false)),
        _ => None,
    };
    if let Some(index) = moved {
        app.decisions_panel.selected = Some(keys[index].clone());
        return None;
    }
    let selected = keys[position].clone();
    app.decisions_panel.selected = Some(selected.clone());
    if key.code == KeyCode::Enter {
        return start_review(app);
    }
    match plain_char(key)? {
        'j' => app.decisions_panel.selected = Some(keys[step(1)].clone()),
        'k' => app.decisions_panel.selected = Some(keys[step(-1)].clone()),
        letter @ 'a'..='i' => return pick(app, &selected, letter),
        'w' => open_editor(app, &selected, EditorKind::Own),
        '?' => open_editor(app, &selected, EditorKind::Clarify),
        'l' => return later(app, &selected),
        'x' => return clear(app, &selected),
        'A' => return accept_leans(app),
        'S' => return start_review(app),
        _ => {}
    }
    None
}

fn group_step(app: &App, keys: &[ItemKey], position: usize, forward: bool) -> usize {
    let groups = groups(&app.decisions, &app.children);
    let mut starts = Vec::new();
    let mut at = 0;
    for group in &groups {
        starts.push(at);
        at += group.items.len();
    }
    let current = starts
        .iter()
        .rposition(|start| *start <= position)
        .unwrap_or(0);
    let target = if forward {
        (current + 1) % starts.len()
    } else {
        (current + starts.len() - 1) % starts.len()
    };
    starts.get(target).copied().unwrap_or(0).min(keys.len() - 1)
}

fn decision_mut<'a>(app: &'a mut App, key: &DecisionKey) -> Option<&'a mut Decision> {
    app.decisions
        .iter_mut()
        .find(|decision| &decision.key == key)
}

fn draft(app: &mut App, key: &DecisionKey, answer: Option<Answer>) -> Option<AppEvent> {
    let decision = decision_mut(app, key)?;
    decision.draft = answer.clone().map(|answer| Draft {
        answer,
        revision: decision.revision,
    });
    Some(AppEvent::DecisionDrafts(vec![(key.clone(), answer)]))
}

fn pick(app: &mut App, selected: &ItemKey, letter: char) -> Option<AppEvent> {
    let index = usize::from(u8::try_from(letter).ok()? - b'a');
    match selected {
        ItemKey::Confirm(child) => {
            let allow = match index {
                0 => true,
                1 => false,
                _ => return None,
            };
            let confirms = &mut app.decisions_panel.confirms;
            if confirms.get(child) == Some(&allow) {
                confirms.remove(child);
            } else {
                confirms.insert(child.clone(), allow);
            }
            None
        }
        ItemKey::Decision(key) => {
            let decision = app.decisions.iter().find(|decision| &decision.key == key)?;
            let label = decision.options.get(index)?.label.clone();
            let (same, note) = match &decision.draft {
                Some(Draft {
                    answer: Answer::Pick { option, note, .. },
                    ..
                }) => (*option == index && !decision.draft_is_stale(), note.clone()),
                _ => (false, None),
            };
            let answer = (!same).then_some(Answer::Pick {
                option: index,
                label,
                note,
            });
            draft(app, key, answer)
        }
    }
}

fn open_editor(app: &mut App, selected: &ItemKey, kind: EditorKind) {
    let ItemKey::Decision(key) = selected else {
        app.decisions_panel.notice = Some("a tool call takes a) allow or b) deny".into());
        return;
    };
    let Some(decision) = app.decisions.iter().find(|decision| &decision.key == key) else {
        return;
    };
    let (kind, text) = match (&decision.draft, kind) {
        (
            Some(Draft {
                answer: Answer::Pick { note, .. },
                ..
            }),
            EditorKind::Own,
        ) => (EditorKind::Note, note.clone().unwrap_or_default()),
        (
            Some(Draft {
                answer: Answer::Own { text },
                ..
            }),
            EditorKind::Own,
        ) => (EditorKind::Own, text.clone()),
        (
            Some(Draft {
                answer: Answer::Clarify { question },
                ..
            }),
            EditorKind::Clarify,
        ) => (EditorKind::Clarify, question.clone()),
        (_, kind) => (kind, String::new()),
    };
    app.decisions_panel.editor = Some(Editor::new(kind, text));
}

fn later(app: &mut App, selected: &ItemKey) -> Option<AppEvent> {
    let ItemKey::Decision(key) = selected else {
        return None;
    };
    let decision = app.decisions.iter().find(|decision| &decision.key == key)?;
    let already = matches!(
        decision.draft,
        Some(Draft {
            answer: Answer::Later,
            ..
        })
    );
    draft(app, key, (!already).then_some(Answer::Later))
}

fn clear(app: &mut App, selected: &ItemKey) -> Option<AppEvent> {
    match selected {
        ItemKey::Confirm(child) => {
            app.decisions_panel.confirms.remove(child);
            None
        }
        ItemKey::Decision(key) => draft(app, key, None),
    }
}

fn accept_leans(app: &mut App) -> Option<AppEvent> {
    let mut drafts = Vec::new();
    for decision in &mut app.decisions {
        if !decision.waiting_on_user() || decision.draft.is_some() {
            continue;
        }
        let Some(option) = decision.lean else {
            continue;
        };
        let Some(label) = decision
            .options
            .get(option)
            .map(|option| option.label.clone())
        else {
            continue;
        };
        let answer = Answer::Pick {
            option,
            label,
            note: None,
        };
        decision.draft = Some(Draft {
            answer: answer.clone(),
            revision: decision.revision,
        });
        drafts.push((decision.key.clone(), Some(answer)));
    }
    if drafts.is_empty() {
        app.decisions_panel.notice =
            Some("every decision with a lean already has an answer".into());
        return None;
    }
    app.decisions_panel.notice = Some(format!(
        "took the lean on {} decision{}",
        drafts.len(),
        if drafts.len() == 1 { "" } else { "s" }
    ));
    Some(AppEvent::DecisionDrafts(drafts))
}

fn start_review(app: &mut App) -> Option<AppEvent> {
    if outgoing(app).is_empty() {
        app.decisions_panel.notice =
            Some("nothing to send yet: pick an option, write an answer or ask a question".into());
    } else {
        app.decisions_panel.review = true;
        app.decisions_panel.review_scroll.set(0);
    }
    None
}

fn review_key(app: &mut App, key: KeyEvent, toggle: bool) -> Option<AppEvent> {
    if toggle {
        app.decisions_panel.review = false;
        app.decisions_panel.open = false;
        return None;
    }
    if app.keymap.matches(Action::Cancel, key) {
        app.decisions_panel.review = false;
        return None;
    }
    let scroll = &app.decisions_panel.review_scroll;
    let moved = match (key.code, plain_char(key)) {
        (KeyCode::Down, _) | (_, Some('j')) => Some(scroll.get().saturating_add(1)),
        (KeyCode::Up, _) | (_, Some('k')) => Some(scroll.get().saturating_sub(1)),
        (KeyCode::PageDown, _) => Some(scroll.get().saturating_add(10)),
        (KeyCode::PageUp, _) => Some(scroll.get().saturating_sub(10)),
        (KeyCode::Home, _) => Some(0),
        (KeyCode::End, _) => Some(usize::MAX),
        _ => None,
    };
    if let Some(offset) = moved {
        scroll.set(offset);
        return None;
    }
    if key.code != KeyCode::Enter {
        return None;
    }
    app.decisions_panel.review = false;
    let out = outgoing(app);
    if out.is_empty() {
        return None;
    }
    for (child, _) in &out.confirms {
        app.decisions_panel.confirms.remove(child);
        if let Some(known) = app.children.iter_mut().find(|known| &known.id == child) {
            known.awaiting_confirmation = None;
        }
    }
    app.decisions_panel.notice = Some(format!(
        "sent {} answer{}",
        out.count(),
        if out.count() == 1 { "" } else { "s" }
    ));
    Some(AppEvent::DecisionsSend {
        keys: out.keys,
        confirms: out.confirms,
    })
}

fn editor_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let cancel = app.keymap.matches(Action::Cancel, key);
    let editor = app.decisions_panel.editor.as_mut()?;
    if cancel {
        app.decisions_panel.editor = None;
        return None;
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => return commit_editor(app),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Left => editor.left(),
        KeyCode::Right => editor.right(),
        KeyCode::Home => editor.cursor = 0,
        KeyCode::End => editor.cursor = editor.text.len(),
        KeyCode::Char('u') if control => {
            editor.text.clear();
            editor.cursor = 0;
        }
        KeyCode::Char(c) if !control => editor.insert(c.encode_utf8(&mut [0; 4])),
        _ => {}
    }
    None
}

fn commit_editor(app: &mut App) -> Option<AppEvent> {
    let editor = app.decisions_panel.editor.take()?;
    let Some(ItemKey::Decision(key)) = app.decisions_panel.selected.clone() else {
        return None;
    };
    let text = editor.text.trim().to_owned();
    let decision = app.decisions.iter().find(|decision| decision.key == key)?;
    let answer = match editor.kind {
        EditorKind::Note => match &decision.draft {
            Some(Draft {
                answer: Answer::Pick { option, label, .. },
                ..
            }) => Answer::Pick {
                option: *option,
                label: label.clone(),
                note: (!text.is_empty()).then_some(text),
            },
            _ => return None,
        },
        _ if text.is_empty() => return None,
        EditorKind::Own => Answer::Own { text },
        EditorKind::Clarify => Answer::Clarify { question: text },
    };
    draft(app, &key, Some(answer))
}

impl App {
    pub fn note_decisions(&mut self, decisions: Vec<Decision>) {
        self.decisions = decisions;
        let groups = groups(&self.decisions, &self.children);
        let keys = flat(&groups);
        if self.decisions_panel.open && keys.is_empty() && self.decisions_panel.editor.is_none() {
            self.decisions_panel.review = false;
        }
    }
}

#[cfg(test)]
mod tests;
