use super::*;

fn root() -> Asker {
    Asker {
        session: "root-s".into(),
        child: None,
        name: "lead".into(),
        depth: 0,
        path: Vec::new(),
        ancestors: Vec::new(),
    }
}

fn child(name: &str) -> Asker {
    Asker {
        session: format!("{name}-s"),
        child: Some(format!("sub-{name}")),
        name: name.into(),
        depth: 1,
        path: Vec::new(),
        ancestors: vec!["root-s".into()],
    }
}

fn grandchild(name: &str, parent: &str) -> Asker {
    Asker {
        session: format!("{name}-s"),
        child: Some(format!("sub-{name}")),
        name: name.into(),
        depth: 2,
        path: vec![parent.into()],
        ancestors: vec!["root-s".into(), format!("{parent}-s")],
    }
}

fn options(labels: &[&str]) -> Vec<DecisionOption> {
    labels
        .iter()
        .map(|label| DecisionOption {
            label: (*label).into(),
            ..Default::default()
        })
        .collect()
}

fn ask(id: &str, question: &str, labels: &[&str], lean: Option<usize>) -> Ask {
    Ask {
        id: id.into(),
        question: question.into(),
        options: options(labels),
        lean: lean.map(Lean::Index),
        ..Ask::default()
    }
}

fn key(asker: &Asker, id: &str) -> DecisionKey {
    DecisionKey {
        session: asker.session.clone(),
        id: id.into(),
    }
}

fn pick(option: usize) -> Answer {
    Answer::Pick {
        option,
        label: String::new(),
        note: None,
    }
}

#[test]
fn a_payload_becomes_an_ask_with_options_lean_and_refs() {
    let ask = Ask::from_payload(&json!({
        "id": "layout",
        "question": "Which layout?",
        "options": ["auto", {"label": "always two rows", "detail": "costs a row"}],
        "lean": "auto",
        "refs": "/tmp/mock.png",
        "blocking": true,
        "assume_after": "2h",
    }))
    .unwrap();
    assert_eq!(ask.id, "layout");
    assert_eq!(ask.options[1].detail.as_deref(), Some("costs a row"));
    assert_eq!(ask.lean, Some(Lean::Label("auto".into())));
    assert_eq!(ask.refs, vec!["/tmp/mock.png".to_string()]);
    assert!(ask.blocking);
    assert_eq!(ask.assume_after, Some(Duration::from_secs(7_200)));
}

#[test]
fn bad_payloads_are_refused_with_a_reason() {
    let cases = [
        (json!({"question": "q"}), "id is required"),
        (json!({"id": "has space", "question": "q"}), "a decision id"),
        (json!({"id": "a"}), "question is required"),
        (
            json!({"id": "a", "question": "q", "options": "x"}),
            "options must be a list",
        ),
        (
            json!({"id": "a", "question": "q", "lean": true}),
            "lean must be",
        ),
        (
            json!({"id": "a", "question": "q", "assume_after": "soon"}),
            "assume_after",
        ),
    ];
    for (payload, expected) in cases {
        let error = Ask::from_payload(&payload).unwrap_err();
        assert!(error.contains(expected), "{payload}: {error}");
    }
}

#[test]
fn asking_validates_options_and_the_lean() {
    let store = Decisions::new(None);
    let too_many: Vec<&str> = vec!["x"; 10];
    let error = store
        .ask(root(), ask("a", "q", &too_many, None))
        .unwrap_err();
    assert!(error.contains("at most 9 options"), "{error}");
    let error = store
        .ask(root(), ask("a", "q", &["x", "X"], None))
        .unwrap_err();
    assert!(error.contains("listed twice"), "{error}");
    let error = store.ask(root(), ask("a", "q", &[], Some(0))).unwrap_err();
    assert!(error.contains("a lean needs options"), "{error}");
    let error = store
        .ask(root(), ask("a", "q", &["x"], Some(3)))
        .unwrap_err();
    assert!(error.contains("out of range"), "{error}");
    let mut by_label = ask("a", "q", &["Thin", "Bars"], None);
    by_label.lean = Some(Lean::Label("bars".into()));
    assert_eq!(store.ask(root(), by_label).unwrap().decision.lean, Some(1));
}

#[test]
fn a_new_decision_is_open_and_counted() {
    let store = Decisions::new(None);
    let before = store.revision();
    let asked = store
        .ask(
            child("statusbar"),
            ask("layout", "Which layout?", &["auto", "two"], Some(0)),
        )
        .unwrap();
    assert!(!asked.updated);
    assert_eq!(asked.open_with_user, 1);
    assert!(asked.decision.waiting_on_user());
    assert!(store.revision() > before);
    assert_eq!(store.visible().len(), 1);
    assert_eq!(store.open_with_user(), 1);
}

#[test]
fn asking_the_same_id_again_updates_it_and_keeps_a_picked_draft_by_label() {
    let store = Decisions::new(None);
    let asker = child("statusbar");
    store
        .ask(
            asker.clone(),
            ask("layout", "Which?", &["auto", "two"], Some(0)),
        )
        .unwrap();
    store
        .set_draft(&key(&asker, "layout"), Some(pick(1)))
        .unwrap();
    let asked = store
        .ask(
            asker.clone(),
            ask("layout", "Which layout?", &["one", "auto", "two"], None),
        )
        .unwrap();
    assert!(asked.updated);
    let decision = &store.visible()[0];
    assert_eq!(decision.question, "Which layout?");
    assert_eq!(decision.revision, 2);
    assert_eq!(
        decision.draft.as_ref().unwrap().answer,
        Answer::Pick {
            option: 2,
            label: "two".into(),
            note: None
        }
    );
    assert!(!decision.draft_is_stale());
}

#[test]
fn a_draft_whose_option_vanished_turns_into_own_words_and_is_flagged() {
    let store = Decisions::new(None);
    let asker = root();
    store
        .ask(asker.clone(), ask("x", "Which?", &["red", "blue"], None))
        .unwrap();
    store.set_draft(&key(&asker, "x"), Some(pick(1))).unwrap();
    store
        .update(
            &asker.session,
            "x",
            Change {
                options: Some(options(&["green"])),
                ..Change::default()
            },
        )
        .unwrap();
    let decision = &store.visible()[0];
    assert_eq!(
        decision.draft.as_ref().unwrap().answer,
        Answer::Own {
            text: "blue".into()
        }
    );
    assert!(decision.draft_is_stale());
}

#[test]
fn an_update_keeps_the_lean_when_its_label_survives() {
    let store = Decisions::new(None);
    let asker = root();
    store
        .ask(asker.clone(), ask("x", "Which?", &["red", "blue"], Some(1)))
        .unwrap();
    let decision = store
        .update(
            &asker.session,
            "x",
            Change {
                options: Some(options(&["blue", "green"])),
                ..Change::default()
            },
        )
        .unwrap();
    assert_eq!(decision.lean, Some(0));
    let error = store
        .update(&asker.session, "missing", Change::default())
        .unwrap_err();
    assert!(error.contains("no open decision missing"), "{error}");
}

#[test]
fn an_agent_may_keep_only_so_many_open() {
    let store = Decisions::new(None);
    for index in 0..MAX_OPEN_PER_AGENT {
        store
            .ask(root(), ask(&format!("d{index}"), "q", &[], None))
            .unwrap();
    }
    let error = store
        .ask(root(), ask("one-more", "q", &[], None))
        .unwrap_err();
    assert!(error.contains("withdraw"), "{error}");
    store
        .ask(child("other"), ask("one-more", "q", &[], None))
        .unwrap();
}

#[test]
fn a_draft_checks_the_option_and_text() {
    let store = Decisions::new(None);
    let asker = root();
    store
        .ask(asker.clone(), ask("x", "Which?", &["red"], None))
        .unwrap();
    let error = store
        .set_draft(&key(&asker, "x"), Some(pick(4)))
        .unwrap_err();
    assert!(error.contains("no option e"), "{error}");
    let error = store
        .set_draft(&key(&asker, "x"), Some(Answer::Own { text: "  ".into() }))
        .unwrap_err();
    assert!(error.contains("needs some text"), "{error}");
    store.set_draft(&key(&asker, "x"), Some(pick(0))).unwrap();
    store.set_draft(&key(&asker, "x"), None).unwrap();
    assert!(store.visible()[0].draft.is_none());
}

#[test]
fn sending_answers_groups_them_by_agent_and_tells_each_parent() {
    let store = Decisions::new(None);
    let statusbar = child("statusbar");
    let helper = grandchild("helper", "statusbar");
    store
        .ask(
            statusbar.clone(),
            ask("layout", "Which layout?", &["auto", "two rows"], Some(0)),
        )
        .unwrap();
    store
        .ask(
            statusbar.clone(),
            ask("idle", "Show the last turn?", &["yes", "no"], Some(0)),
        )
        .unwrap();
    store
        .ask(
            statusbar.clone(),
            ask("meters", "Thin meters?", &["thin", "bars"], Some(0)),
        )
        .unwrap();
    store
        .ask(
            helper.clone(),
            ask("stale", "Which models stay?", &[], None),
        )
        .unwrap();
    store
        .set_draft(
            &key(&statusbar, "layout"),
            Some(Answer::Pick {
                option: 0,
                label: String::new(),
                note: Some("keep 140 as the line".into()),
            }),
        )
        .unwrap();
    store
        .set_draft(
            &key(&statusbar, "idle"),
            Some(Answer::Own {
                text: "only when it took over a minute".into(),
            }),
        )
        .unwrap();
    store
        .set_draft(&key(&statusbar, "meters"), Some(Answer::Later))
        .unwrap();
    store
        .set_draft(
            &key(&helper, "stale"),
            Some(Answer::Clarify {
                question: "unused or deprecated?".into(),
            }),
        )
        .unwrap();
    let keys: Vec<DecisionKey> = store.visible().iter().map(|d| d.key.clone()).collect();
    let outgoing = store.send(&keys).unwrap();
    assert_eq!(outgoing.len(), 2);
    let to_statusbar = &outgoing[0];
    assert_eq!(to_statusbar.asker.name, "statusbar");
    assert_eq!(
        to_statusbar.text,
        "Answers to your decisions:\n\n\
         [layout] Which layout?\n\
         a) auto (your lean). Note: keep 140 as the line\n\n\
         [idle] Show the last turn?\n\
         In my own words: only when it took over a minute\n\n\
         Still open with me: [meters]."
    );
    let to_helper = &outgoing[1];
    assert!(
        to_helper
            .text
            .contains("Not yet, I need more first: unused or deprecated?"),
        "{}",
        to_helper.text
    );
    assert!(to_helper.text.contains("rlm.decisions.update(\"stale\""));

    let visible = store.visible();
    let ids: Vec<&str> = visible.iter().map(|d| d.key.id.as_str()).collect();
    assert_eq!(ids, ["meters", "stale"], "later stays, clarifying stays");
    assert!(matches!(
        visible[1].status,
        DecisionStatus::Clarifying { .. }
    ));
    assert_eq!(store.open_with_user(), 1);

    let to_root = store.take_news("root-s").unwrap();
    assert!(to_root.contains("- the user answered statusbar's [layout]: a) auto"));
    assert!(to_root.contains("- the user answered statusbar's [idle]: In my own words"));
    assert!(
        !to_root.contains("helper"),
        "only the immediate parent is told: {to_root}"
    );
    let to_statusbar_news = store.take_news("statusbar-s").unwrap();
    assert!(
        to_statusbar_news.contains("the user answered helper's [stale]: asked them to clarify")
    );
}

#[test]
fn undelivered_answers_arrive_with_the_next_turn_and_only_once() {
    let store = Decisions::new(None);
    let asker = child("worker");
    store
        .ask(asker.clone(), ask("x", "Which?", &["a", "b"], None))
        .unwrap();
    store.set_draft(&key(&asker, "x"), Some(pick(1))).unwrap();
    let outgoing = store.send(&[key(&asker, "x")]).unwrap();
    store.mark_delivered(&outgoing[0].keys, false);
    let news = store.take_news("worker-s").unwrap();
    assert!(news.starts_with("Answers to your decisions:"), "{news}");
    assert!(news.contains("b) b"));
    assert_eq!(store.take_news("worker-s"), None);
}

#[test]
fn a_clarified_decision_comes_back_open_when_the_agent_updates_it() {
    let store = Decisions::new(None);
    let asker = root();
    store
        .ask(asker.clone(), ask("x", "Which?", &["a", "b"], None))
        .unwrap();
    store
        .set_draft(
            &key(&asker, "x"),
            Some(Answer::Clarify {
                question: "what is b?".into(),
            }),
        )
        .unwrap();
    store.send(&[key(&asker, "x")]).unwrap();
    assert_eq!(store.open_with_user(), 0);
    let decision = store
        .update(
            &asker.session,
            "x",
            Change {
                context: Some("b is the faster one".into()),
                ..Change::default()
            },
        )
        .unwrap();
    assert!(decision.waiting_on_user());
    assert_eq!(decision.clarified.as_deref(), Some("what is b?"));
    assert_eq!(store.open_with_user(), 1);
}

#[test]
fn withdrawing_closes_it_and_superseding_carries_the_draft() {
    let store = Decisions::new(None);
    let asker = root();
    store
        .ask(asker.clone(), ask("old", "Which?", &["a", "b"], None))
        .unwrap();
    store.set_draft(&key(&asker, "old"), Some(pick(1))).unwrap();
    let mut newer = ask("new", "Which, now?", &["b", "c"], None);
    newer.supersedes = Some("old".into());
    store.ask(asker.clone(), newer).unwrap();
    let visible = store.visible();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].key.id, "new");
    assert_eq!(
        visible[0].draft.as_ref().unwrap().answer,
        Answer::Pick {
            option: 0,
            label: "b".into(),
            note: None
        }
    );
    let withdrawn = store
        .withdraw(&asker.session, "new", Some("answered in chat".into()))
        .unwrap();
    assert!(withdrawn.is_closed());
    assert!(store.visible().is_empty());
    let listed = store.list(Scope::Mine("root-s"));
    assert_eq!(listed.len(), 2);
    let error = store.withdraw(&asker.session, "new", None).unwrap_err();
    assert!(error.contains("no open decision new"), "{error}");
}

#[test]
fn the_family_scope_covers_the_agents_below() {
    let store = Decisions::new(None);
    store.ask(root(), ask("r", "q", &[], None)).unwrap();
    store.ask(child("a"), ask("x", "q", &[], None)).unwrap();
    store
        .ask(grandchild("b", "a"), ask("y", "q", &[], None))
        .unwrap();
    store.ask(child("c"), ask("z", "q", &[], None)).unwrap();
    let ids = |scope| -> Vec<String> {
        store
            .list(scope)
            .into_iter()
            .map(|decision| decision.key.id)
            .collect()
    };
    assert_eq!(ids(Scope::Mine("a-s")), ["x"]);
    assert_eq!(ids(Scope::Family("a-s")), ["x", "y"]);
    assert_eq!(ids(Scope::Family("root-s")), ["r", "x", "y", "z"]);
}

#[test]
fn the_digest_lists_open_decisions_and_those_below() {
    let store = Decisions::new(None);
    assert_eq!(store.digest("a-s"), None);
    let mut blocking = ask("x", "Push now?", &["push", "hold"], Some(1));
    blocking.blocking = true;
    store.ask(child("a"), blocking).unwrap();
    store
        .ask(grandchild("b", "a"), ask("y", "Which shells?", &[], None))
        .unwrap();
    let digest = store.digest("a-s").unwrap();
    assert!(
        digest.contains("- [x] Push now? Options: a) push, b) hold. Your lean: b. Blocking."),
        "{digest}"
    );
    assert!(
        digest.contains("- a \u{203a} b [y] Which shells?"),
        "{digest}"
    );
    let message = digest_after_compaction(&store, "a-s", &[]).unwrap();
    assert!(digest_after_compaction(&store, "a-s", std::slice::from_ref(&message)).is_none());
}

#[tokio::test]
async fn waiting_returns_once_the_user_has_answered() {
    let store = Decisions::new(None);
    let asker = root();
    store
        .ask(asker.clone(), ask("x", "Which?", &["a", "b"], None))
        .unwrap();
    let waiter = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .wait("root-s", &["x".to_string()], Duration::from_secs(10))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    store.set_draft(&key(&asker, "x"), Some(pick(0))).unwrap();
    store.send(&[key(&asker, "x")]).unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        answered[0].status,
        DecisionStatus::Answered { .. }
    ));
    let timed_out = store
        .wait("root-s", &["x".to_string()], Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(timed_out.len(), 1);
    let error = store
        .wait("root-s", &["nope".to_string()], Duration::ZERO)
        .await
        .unwrap_err();
    assert!(error.contains("no decision nope"), "{error}");
}

#[test]
fn the_store_survives_a_reload_and_logs_each_change() {
    let dir = tempfile::tempdir().unwrap();
    let store = Decisions::new(Some(dir.path().to_path_buf()));
    let asker = root();
    store
        .ask(asker.clone(), ask("x", "Which?", &["a", "b"], Some(0)))
        .unwrap();
    store.set_draft(&key(&asker, "x"), Some(pick(1))).unwrap();
    let reloaded = Decisions::new(Some(dir.path().to_path_buf()));
    assert_eq!(reloaded.visible(), store.visible());
    store.withdraw("root-s", "x", None).unwrap();
    let log = std::fs::read_to_string(dir.path().join("decisions.jsonl")).unwrap();
    let events: Vec<String> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["event"].to_string())
        .collect();
    assert_eq!(events, ["\"asked\"", "\"withdrawn\""]);
    assert!(!log.contains("\"draft\""), "drafts stay out of the log");

    let other = tempfile::tempdir().unwrap();
    store.rebind(Some(other.path().to_path_buf()));
    assert!(store.visible().is_empty());
    store.rebind(Some(dir.path().to_path_buf()));
    assert_eq!(store.list(Scope::Mine("root-s")).len(), 1);
}

#[test]
fn closed_decisions_are_pruned_oldest_first() {
    let store = Decisions::new(None);
    let asker = child("busy");
    for round in 0..(KEEP_CLOSED + 5) {
        let id = format!("d{round}");
        store.ask(asker.clone(), ask(&id, "q", &[], None)).unwrap();
        store.withdraw(&asker.session, &id, None).unwrap();
    }
    let kept = store.list(Scope::Mine("busy-s"));
    assert_eq!(kept.len(), KEEP_CLOSED);
    assert_eq!(kept[0].key.id, "d5");
}

#[test]
fn the_agent_view_reads_like_the_menu() {
    let store = Decisions::new(None);
    let asker = grandchild("helper", "statusbar");
    store
        .ask(asker.clone(), ask("x", "Which?", &["red", "blue"], Some(1)))
        .unwrap();
    store.set_draft(&key(&asker, "x"), Some(pick(1))).unwrap();
    store.send(&[key(&asker, "x")]).unwrap();
    let view = agent_view(&store.list(Scope::Mine("helper-s"))[0]);
    assert_eq!(view["asker"], "statusbar \u{203a} helper");
    assert_eq!(view["status"], "answered");
    assert_eq!(view["answer"], "b) blue (your lean)");
    assert_eq!(view["lean"], "blue");
}

#[test]
fn the_digest_caps_what_it_lists_from_below() {
    let store = Decisions::new(None);
    for index in 0..15 {
        store
            .ask(
                child(&format!("c{index}")),
                ask("x", &format!("question {index}?"), &[], None),
            )
            .unwrap();
    }
    let digest = store.digest("root-s").unwrap();
    assert_eq!(digest.matches("\n- c").count(), DIGEST_BELOW, "{digest}");
    assert!(digest.contains("- and 3 more"), "{digest}");
}

#[test]
fn options_take_pros_and_cons_as_lists_or_single_strings() {
    let payload = serde_json::json!({
        "id": "push",
        "question": "Push main now?",
        "options": [
            {"label": "push now", "pros": ["unblocks the review", " "], "cons": "skips the soak test"},
            "hold",
        ],
    });
    let ask = Ask::from_payload(&payload).unwrap();
    assert_eq!(ask.options[0].pros, ["unblocks the review"]);
    assert_eq!(ask.options[0].cons, ["skips the soak test"]);
    assert!(ask.options[1].pros.is_empty());

    let saved = serde_json::to_value(&ask.options[1]).unwrap();
    assert_eq!(saved, serde_json::json!({"label": "hold"}));
    let read: DecisionOption =
        serde_json::from_value(serde_json::json!({"label": "hold"})).unwrap();
    assert!(read.cons.is_empty());
}

#[test]
fn pros_and_cons_are_capped() {
    let store = Decisions::new(None);
    let mut many = ask("push", "Push main now?", &["push now", "hold"], None);
    many.options[0].pros = (0..=MAX_POINTS)
        .map(|point| format!("pro {point}"))
        .collect();
    let error = store.ask(child("c"), many).unwrap_err();
    assert!(error.contains("at most"), "{error}");

    let mut long = ask("push", "Push main now?", &["push now", "hold"], None);
    long.options[1].cons = vec!["x".repeat(MAX_POINT_CHARS + 1)];
    let error = store.ask(child("c"), long).unwrap_err();
    assert!(error.contains("longer than"), "{error}");

    let bad = serde_json::json!({
        "id": "push",
        "question": "Push main now?",
        "options": [{"label": "push now", "pros": 3}],
    });
    assert!(Ask::from_payload(&bad).is_err());
}
