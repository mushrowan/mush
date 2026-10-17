use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mush_ai::types::{Timestamp, TokenCount};
use mush_runtime::{Asker, DecisionOption};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::test_support::buffer_to_string;

fn asker(name: &str, root: bool) -> Asker {
    Asker {
        session: format!("{name}-s"),
        child: (!root).then(|| format!("sub-{name}")),
        name: name.into(),
        depth: u32::from(!root),
        path: Vec::new(),
        ancestors: if root {
            Vec::new()
        } else {
            vec!["lead-s".into()]
        },
    }
}

fn decision(
    by: &Asker,
    id: &str,
    question: &str,
    options: &[&str],
    lean: Option<usize>,
) -> Decision {
    Decision {
        key: DecisionKey {
            session: by.session.clone(),
            id: id.into(),
        },
        asker: by.clone(),
        question: question.into(),
        options: options
            .iter()
            .map(|label| DecisionOption {
                label: (*label).into(),
                detail: None,
            })
            .collect(),
        lean,
        context: None,
        refs: Vec::new(),
        blocking: false,
        assume_at: None,
        status: DecisionStatus::Open,
        asked_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        revision: 1,
        draft: None,
        delivered: false,
        clarified: None,
    }
}

fn child(name: &str, confirmation: Option<&str>) -> ChildSummary {
    ChildSummary {
        paused: None,
        context: None,
        id: format!("sub-{name}"),
        name: name.into(),
        parent: None,
        depth: 1,
        session_id: format!("{name}-s"),
        model_id: "m".into(),
        thinking: None,
        status: mush_runtime::ChildStatus::Running,
        spawned_at: Timestamp::zero(),
        updated_at: Timestamp::now(),
        idle_since: Timestamp::zero(),
        answer: None,
        error: None,
        progress_note: None,
        usage: Default::default(),
        cost: Default::default(),
        turns: 1,
        streaming: false,
        tool_running: None,
        awaiting_confirmation: confirmation.map(Into::into),
    }
}

fn app() -> App {
    let mut app = App::new("claude-opus-5-5".into(), TokenCount::new(200_000));
    let lead = asker("lead", true);
    let statusbar = asker("statusbar", false);
    let models = asker("models", false);
    let mut fold = decision(
        &models,
        "fold-3",
        "Rerun fold 3 on the 40% split?",
        &["rerun", "keep"],
        Some(0),
    );
    fold.blocking = true;
    app.decisions = vec![
        decision(
            &statusbar,
            "layout",
            "Default status bar layout?",
            &["auto", "two rows", "one row"],
            Some(0),
        ),
        decision(
            &lead,
            "push",
            "Push main to origin now?",
            &["push now", "hold"],
            Some(1),
        ),
        decision(
            &statusbar,
            "shells",
            "Which shells get completions?",
            &[],
            None,
        ),
        fold,
    ];
    app
}

fn press(app: &mut App, code: KeyCode) -> Option<AppEvent> {
    crate::input::handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(app: &mut App, c: char) -> Option<AppEvent> {
    crate::input::handle_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn select(app: &mut App, id: &str) {
    let key = app
        .decisions
        .iter()
        .find(|decision| decision.key.id == id)
        .unwrap()
        .key
        .clone();
    app.decisions_panel.selected = Some(ItemKey::Decision(key));
}

fn draft_of<'a>(app: &'a App, id: &str) -> Option<&'a Answer> {
    app.decisions
        .iter()
        .find(|decision| decision.key.id == id)
        .and_then(|decision| decision.draft.as_ref())
        .map(|draft| &draft.answer)
}

#[test]
fn ctrl_q_opens_and_closes_the_panel_and_asks_for_a_fresh_list() {
    let mut app = app();
    assert!(matches!(
        ctrl(&mut app, 'q'),
        Some(AppEvent::DecisionsRefresh)
    ));
    assert!(app.decisions_panel.open);
    assert!(ctrl(&mut app, 'q').is_none());
    assert!(!app.decisions_panel.open);
    ctrl(&mut app, 'q');
    press(&mut app, KeyCode::Esc);
    assert!(!app.decisions_panel.open);
    assert!(app.input.text.is_empty(), "no key reached the composer");
}

#[test]
fn the_root_comes_first_then_blocking_agents_then_the_rest() {
    let app = app();
    let groups = groups(&app.decisions, &app.children);
    let labels: Vec<&str> = groups.iter().map(|group| group.label.as_str()).collect();
    assert_eq!(labels, ["lead", "models", "statusbar"]);
    assert!(groups[0].root);
    let ids: Vec<&str> = groups[2].items.iter().map(Item::id).collect();
    assert_eq!(ids, ["layout", "shells"]);
}

#[test]
fn a_letter_picks_an_option_and_the_same_letter_clears_it() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "layout");
    let event = press(&mut app, KeyCode::Char('b'));
    let Some(AppEvent::DecisionDrafts(drafts)) = event else {
        panic!("expected a draft, got {event:?}");
    };
    assert_eq!(drafts.len(), 1);
    assert_eq!(
        drafts[0].1,
        Some(Answer::Pick {
            option: 1,
            label: "two rows".into(),
            note: None
        })
    );
    assert!(matches!(
        draft_of(&app, "layout"),
        Some(Answer::Pick { option: 1, .. })
    ));
    press(&mut app, KeyCode::Char('b'));
    assert_eq!(draft_of(&app, "layout"), None);
    assert!(press(&mut app, KeyCode::Char('f')).is_none(), "no option f");
}

#[test]
fn w_writes_an_own_answer_or_a_note_on_a_pick() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "shells");
    press(&mut app, KeyCode::Char('w'));
    assert_eq!(
        app.decisions_panel.editor.as_ref().unwrap().kind,
        EditorKind::Own
    );
    for c in "bash and fish".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    let event = press(&mut app, KeyCode::Enter);
    assert!(matches!(event, Some(AppEvent::DecisionDrafts(_))));
    assert_eq!(
        draft_of(&app, "shells"),
        Some(&Answer::Own {
            text: "bash and fish".into()
        })
    );

    select(&mut app, "layout");
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('w'));
    assert_eq!(
        app.decisions_panel.editor.as_ref().unwrap().kind,
        EditorKind::Note
    );
    app.handle_paste("keep 140\nas the line");
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        draft_of(&app, "layout"),
        Some(&Answer::Pick {
            option: 0,
            label: "auto".into(),
            note: Some("keep 140 as the line".into())
        })
    );
}

#[test]
fn a_question_mark_asks_for_clarification_and_esc_drops_the_edit() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "push");
    press(&mut app, KeyCode::Char('?'));
    for c in "which remote?".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        draft_of(&app, "push"),
        Some(&Answer::Clarify {
            question: "which remote".into()
        })
    );
    press(&mut app, KeyCode::Char('?'));
    press(&mut app, KeyCode::Char('!'));
    press(&mut app, KeyCode::Esc);
    assert!(app.decisions_panel.editor.is_none());
    assert!(
        app.decisions_panel.open,
        "esc in the editor only closes the editor"
    );
    assert_eq!(
        draft_of(&app, "push"),
        Some(&Answer::Clarify {
            question: "which remote".into()
        })
    );
}

#[test]
fn later_and_clear_toggle_the_draft() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "push");
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(draft_of(&app, "push"), Some(&Answer::Later));
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(draft_of(&app, "push"), None);
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(draft_of(&app, "push"), None);
}

#[test]
fn capital_a_takes_every_lean_that_has_no_answer_yet() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "push");
    press(&mut app, KeyCode::Char('a'));
    let event = press(&mut app, KeyCode::Char('A'));
    let Some(AppEvent::DecisionDrafts(drafts)) = event else {
        panic!("expected drafts, got {event:?}");
    };
    let ids: Vec<&str> = drafts.iter().map(|(key, _)| key.id.as_str()).collect();
    assert_eq!(ids, ["layout", "fold-3"]);
    assert!(matches!(
        draft_of(&app, "push"),
        Some(Answer::Pick { option: 0, .. })
    ));
    assert_eq!(draft_of(&app, "shells"), None, "no lean, no answer");
    assert_eq!(
        app.decisions_panel.notice.as_deref(),
        Some("took the lean on 2 decisions")
    );
}

#[test]
fn sending_goes_through_a_review_and_leaves_later_out() {
    let mut app = app();
    app.children = vec![child("models", Some("bash rm -rf build"))];
    ctrl(&mut app, 'q');
    press(&mut app, KeyCode::Char('S'));
    assert!(!app.decisions_panel.review);
    assert!(
        app.decisions_panel
            .notice
            .as_deref()
            .unwrap()
            .starts_with("nothing to send yet")
    );

    select(&mut app, "push");
    press(&mut app, KeyCode::Char('b'));
    select(&mut app, "layout");
    press(&mut app, KeyCode::Char('l'));
    app.decisions_panel.selected = Some(ItemKey::Confirm("sub-models".into()));
    press(&mut app, KeyCode::Char('b'));
    assert_eq!(app.decisions_panel.confirms.get("sub-models"), Some(&false));

    press(&mut app, KeyCode::Char('S'));
    assert!(app.decisions_panel.review);
    assert!(
        press(&mut app, KeyCode::Char('j')).is_none(),
        "the review only takes enter or esc"
    );
    let event = press(&mut app, KeyCode::Enter);
    let Some(AppEvent::DecisionsSend { keys, confirms }) = event else {
        panic!("expected a send, got {event:?}");
    };
    let ids: Vec<&str> = keys.iter().map(|key| key.id.as_str()).collect();
    assert_eq!(ids, ["push"]);
    assert_eq!(confirms, vec![("sub-models".to_string(), false)]);
    assert!(!app.decisions_panel.review);
    assert!(app.children[0].awaiting_confirmation.is_none());
    assert_eq!(
        app.decisions_panel.notice.as_deref(),
        Some("sent 2 answers")
    );
}

#[test]
fn a_tool_confirmation_is_a_blocking_item_in_its_agents_group() {
    let mut app = app();
    app.children = vec![child("statusbar", Some("bash cargo publish"))];
    let groups = groups(&app.decisions, &app.children);
    let statusbar = groups
        .iter()
        .find(|group| group.label == "statusbar")
        .unwrap();
    assert!(matches!(statusbar.items[0], Item::Confirm(_)));
    assert_eq!(statusbar.items[0].options(), ["allow", "deny"]);
    assert_eq!(counts(&app), (5, 2));
    ctrl(&mut app, 'q');
    app.decisions_panel.selected = Some(ItemKey::Confirm("sub-statusbar".into()));
    press(&mut app, KeyCode::Char('w'));
    assert!(app.decisions_panel.editor.is_none());
    assert!(
        app.decisions_panel
            .notice
            .as_deref()
            .unwrap()
            .contains("allow or b) deny")
    );
}

#[test]
fn a_draft_the_agent_changed_shows_as_changed() {
    let mut app = app();
    app.decisions[0].draft = Some(Draft {
        answer: Answer::Own {
            text: "two rows".into(),
        },
        revision: 0,
    });
    let groups = groups(&app.decisions, &app.children);
    let layout = groups
        .iter()
        .flat_map(|group| group.items.iter())
        .find(|item| item.id() == "layout")
        .unwrap();
    assert_eq!(chip(layout, &app.decisions_panel), Chip::Changed);
}

fn render(app: &App, width: u16, height: u16) -> (String, Option<(u16, u16)>) {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    let cursor = crate::widgets::decisions::render(app, area, &mut buf);
    (buffer_to_string(&buf), cursor)
}

#[test]
fn the_narrow_panel_opens_the_focused_decision_in_place() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "layout");
    press(&mut app, KeyCode::Char('a'));
    let (text, cursor) = render(&app, 80, 24);
    assert!(cursor.is_none());
    let rows: Vec<&str> = text.lines().collect();
    assert!(rows[0].starts_with("\u{250c} decisions "), "{text}");
    assert!(
        rows[0].contains("4 open \u{b7} 1 drafted \u{b7} 1 blocking"),
        "{text}"
    );
    let push = rows
        .iter()
        .position(|row| row.contains("push     Push main to origin now?"))
        .unwrap();
    let models = rows.iter().position(|row| row.contains("models")).unwrap();
    assert!(push < models, "{text}");
    assert!(rows.iter().any(|row| row.contains("! 2 fold-3")), "{text}");
    assert!(
        rows.iter()
            .any(|row| row.contains("\u{25be} 3 layout") && row.contains("\u{25cf} a lean")),
        "{text}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("\u{25cf} a) auto   \u{25c2} lean")),
        "{text}"
    );
    assert!(
        rows.iter().any(|row| row.contains("w add a note")),
        "{text}"
    );
    assert!(
        rows.iter().any(|row| row.contains("\u{25cb} text")),
        "{text}"
    );
}

#[test]
fn the_wide_panel_puts_the_focused_decision_beside_the_list() {
    let mut app = app();
    app.decisions[0].context = Some("One row cuts the note at 180 columns.".into());
    app.decisions[0].refs = vec!["/tmp/mock.png".into()];
    ctrl(&mut app, 'q');
    select(&mut app, "layout");
    let (text, _) = render(&app, 120, 24);
    let rows: Vec<&str> = text.lines().collect();
    let detail = rows
        .iter()
        .find(|row| row.contains("statusbar \u{b7} layout"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(detail.contains("asked"), "{detail}");
    assert!(
        rows.iter()
            .any(|row| row.contains("\u{2502} One row cuts the note at 180 columns.")),
        "{text}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("\u{2197} /tmp/mock.png")),
        "{text}"
    );
    assert!(
        rows.iter().any(|row| row.contains("\u{25b8} 3 layout")),
        "{text}"
    );
}

#[test]
fn the_editor_puts_the_cursor_after_the_text() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "push");
    press(&mut app, KeyCode::Char('?'));
    for c in "why".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    let (text, cursor) = render(&app, 80, 24);
    let (x, y) = cursor.expect("a cursor in the editor");
    let row: Vec<char> = text.lines().nth(usize::from(y)).unwrap().chars().collect();
    let before: String = row[..usize::from(x)].iter().collect();
    assert!(before.ends_with("? ask lead: why"), "{before:?}");
}

#[test]
fn the_review_lists_answers_by_agent_and_what_stays_open() {
    let mut app = app();
    app.children = vec![child("statusbar", None)];
    ctrl(&mut app, 'q');
    select(&mut app, "push");
    press(&mut app, KeyCode::Char('b'));
    select(&mut app, "shells");
    press(&mut app, KeyCode::Char('?'));
    for c in "which ones?".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('S'));
    let (text, cursor) = render(&app, 80, 24);
    assert!(cursor.is_none());
    assert!(text.contains(" send 2 answers "), "{text}");
    assert!(text.contains("idle, this wakes it"), "{text}");
    assert!(
        text.contains("busy, it gets this at its next step"),
        "{text}"
    );
    assert!(text.contains("b) hold  lean"), "{text}");
    assert!(text.contains("asks: \"which ones?\""), "{text}");
    assert!(
        text.contains("still open: 2 fold-3 (blocking), 3 layout"),
        "{text}"
    );
}

#[test]
fn an_empty_panel_says_nothing_is_waiting() {
    let mut app = App::new("m".into(), TokenCount::new(200_000));
    app.decisions_panel.open = true;
    let (text, _) = render(&app, 60, 8);
    assert!(text.contains("nothing is waiting on you"), "{text}");
}

#[test]
fn a_decision_past_its_assume_by_time_shows_it_is_on_its_lean() {
    let mut app = app();
    app.decisions[1].assume_at = Some(Timestamp::from_ms(1));
    let groups = groups(&app.decisions, &app.children);
    let push = &groups[0].items[0];
    assert_eq!(push.id(), "push");
    assert_eq!(chip(push, &app.decisions_panel), Chip::OnLean);
}

fn crowded() -> App {
    let mut app = app();
    let more = asker("extra", false);
    for index in 0..12 {
        app.decisions.push(decision(
            &more,
            &format!("q{index}"),
            &format!("Question {index}?"),
            &["yes", "no"],
            Some(0),
        ));
    }
    app
}

fn on_screen(app: &App, text: &str) -> (usize, usize, usize) {
    let keys = flat(&groups(&app.decisions, &app.children));
    let visible = keys
        .iter()
        .enumerate()
        .filter(|(index, key)| {
            let id = match key {
                ItemKey::Decision(key) => key.id.as_str(),
                ItemKey::Confirm(_) => "tool",
            };
            text.contains(&format!("{} {id:<8} ", index + 1))
        })
        .count();
    let hidden = |arrow: char| {
        text.lines()
            .find_map(|row| {
                let (_, rest) = row.split_once(&format!("{arrow} "))?;
                rest.split(' ').next()?.parse::<usize>().ok()
            })
            .unwrap_or(0)
    };
    (visible, hidden('\u{2191}'), hidden('\u{2193}'))
}

#[test]
fn a_long_list_keeps_the_focus_on_screen_and_counts_what_is_hidden() {
    let mut app = crowded();
    ctrl(&mut app, 'q');
    let keys = flat(&groups(&app.decisions, &app.children));
    assert_eq!(keys.len(), 16);
    for (width, height) in [(80, 9), (80, 14), (80, 24), (130, 9), (130, 14)] {
        for key in &keys {
            app.decisions_panel.selected = Some(key.clone());
            let (text, _) = render(&app, width, height);
            let ItemKey::Decision(decision) = key else {
                continue;
            };
            let marker = if width >= 110 { '\u{25b8}' } else { '\u{25be}' };
            assert!(
                text.lines()
                    .any(|row| row.contains(marker) && row.contains(decision.id.as_str())),
                "{width}x{height} {}\n{text}",
                decision.id
            );
            let (visible, above, below) = on_screen(&app, &text);
            assert_eq!(
                visible + above + below,
                keys.len(),
                "{width}x{height}\n{text}"
            );
        }
    }
    select(&mut app, "push");
    let (text, _) = render(&app, 80, 14);
    let (_, above, below) = on_screen(&app, &text);
    assert_eq!(above, 0, "{text}");
    assert!(below > 0, "{text}");
    select(&mut app, "q11");
    let (text, _) = render(&app, 80, 14);
    let (_, above, below) = on_screen(&app, &text);
    assert!(above > 0, "{text}");
    assert_eq!(below, 0, "{text}");
}

#[test]
fn a_review_taller_than_the_panel_scrolls() {
    let mut app = crowded();
    ctrl(&mut app, 'q');
    press(&mut app, KeyCode::Char('A'));
    press(&mut app, KeyCode::Char('S'));
    assert!(app.decisions_panel.review);
    let (text, _) = render(&app, 80, 16);
    assert!(text.contains(" send 15 answers "), "{text}");
    assert!(text.contains("j/k scroll"), "{text}");
    assert!(text.contains("\u{2193} "), "{text}");
    assert!(!text.contains("\u{2191} "), "{text}");
    assert!(!text.contains("still open"), "{text}");
    press(&mut app, KeyCode::End);
    let (text, _) = render(&app, 80, 16);
    assert!(text.contains("still open: "), "{text}");
    assert!(text.contains("\u{2191} "), "{text}");
    assert!(!text.contains("\u{2193} "), "{text}");
    let bottom = app.decisions_panel.review_scroll.get();
    press(&mut app, KeyCode::Char('j'));
    render(&app, 80, 16);
    assert_eq!(
        app.decisions_panel.review_scroll.get(),
        bottom,
        "scrolling stops at the end"
    );
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.decisions_panel.review_scroll.get(), bottom - 1);
    press(&mut app, KeyCode::Home);
    let (text, _) = render(&app, 80, 16);
    assert!(!text.contains("\u{2191} "), "{text}");
    let (text, _) = render(&app, 80, 40);
    assert!(!text.contains("j/k scroll"), "{text}");
}

#[test]
fn the_review_says_which_answers_unblock_a_tool_call() {
    let mut app = app();
    app.children = vec![
        child("models", Some("bash rm -rf build")),
        child("statusbar", Some("bash cargo publish")),
    ];
    ctrl(&mut app, 'q');
    app.decisions_panel.selected = Some(ItemKey::Confirm("sub-models".into()));
    press(&mut app, KeyCode::Char('a'));
    select(&mut app, "layout");
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('S'));
    let (text, _) = render(&app, 80, 30);
    let row_with = |state: &str| {
        text.lines()
            .find(|row| row.contains(state))
            .unwrap_or_else(|| panic!("{state}\n{text}"))
            .to_owned()
    };
    assert!(
        row_with("waiting on you, this unblocks it").contains(" models "),
        "{text}"
    );
    assert!(
        row_with("still waiting on its tool call").contains(" statusbar "),
        "{text}"
    );
}

#[test]
fn ctrl_q_in_the_review_closes_the_panel_and_keeps_the_drafts() {
    let mut app = app();
    ctrl(&mut app, 'q');
    select(&mut app, "push");
    press(&mut app, KeyCode::Char('b'));
    press(&mut app, KeyCode::Char('S'));
    assert!(app.decisions_panel.review);
    assert!(ctrl(&mut app, 'q').is_none());
    assert!(!app.decisions_panel.open);
    assert!(!app.decisions_panel.review);
    assert!(matches!(
        draft_of(&app, "push"),
        Some(Answer::Pick { option: 1, .. })
    ));
    ctrl(&mut app, 'q');
    assert!(app.decisions_panel.open);
    assert!(!app.decisions_panel.review, "it reopens on the list");
}
