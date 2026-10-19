use super::*;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        proptest::collection::vec(
            prop_oneof![
                Just("a".to_owned()),
                Just(" \n\t".to_owned()),
                Just("界".to_owned()),
                Just("🦀".to_owned()),
                Just("e\u{301}".to_owned()),
                Just("👩‍💻".to_owned()),
                any::<char>().prop_map(|c| c.to_string()),
            ],
            0..64,
        )
        .prop_map(|parts| parts.concat()),
        (1usize..256).prop_map(|count| "界🦀a".repeat(count)),
    ]
}

fn dimensions() -> impl Strategy<Value = (u16, u16)> {
    prop_oneof![
        (0u16..30, 0u16..9),
        (24u16..160, 6u16..40),
        (1360u16..2048, 6u16..24),
        Just((u16::MAX, 1)),
        Just((1, u16::MAX)),
        Just((0, 0)),
        Just((112, 6)),
    ]
}

fn populated(questions: Vec<(String, Vec<String>, bool)>) -> App {
    let mut app = App::new("m".into(), TokenCount::new(200_000));
    app.decisions_panel.open = true;
    for (index, (question, labels, blocking)) in questions.into_iter().enumerate() {
        let by = asker(&format!("g{}", index % 3), index % 3 == 0);
        let labels = labels.iter().map(String::as_str).collect::<Vec<_>>();
        let mut item = decision(&by, &format!("q{index}"), &question, &labels, None);
        item.blocking = blocking;
        item.context = Some(question.clone());
        item.refs = vec![question.clone()];
        for option in &mut item.options {
            option.detail = Some(question.clone());
            option.pros = vec![question.clone()];
            option.cons = vec![question.clone()];
        }
        app.decisions.push(item);
    }
    app
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(0xdec1510),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_decision_rendering_stays_inside_the_panel(
        questions in proptest::collection::vec((text(), proptest::collection::vec(text(), 0..5), any::<bool>()), 0..6),
        size in dimensions(),
        selected in any::<usize>(),
        scroll in prop_oneof![Just(usize::MAX), 0usize..1024],
        mode in 0u8..5,
        edit in text(),
        confirmations in proptest::collection::vec(text(), 0..3),
    ) {
        let mut app = populated(questions);
        for (index, item) in app.decisions.iter_mut().enumerate() {
            item.asker.name = item.question.clone();
            item.key.id = format!("{index}{}", item.question);
            item.lean = (!item.options.is_empty()).then_some(0);
            item.clarified = Some(edit.clone());
            if index % 2 == 0 {
                item.status = DecisionStatus::Clarifying { question: edit.clone(), at: Timestamp::zero() };
            }
        }
        app.children = confirmations.iter().enumerate().map(|(index, text)| child(&format!("c{index}"), Some(text))).collect();
        let keys = flat(&groups(&app.decisions, &app.children));
        if !keys.is_empty() {
            app.decisions_panel.selected = Some(keys[selected % keys.len()].clone());
        }
        app.decisions_panel.detail_scroll.set(scroll);
        match mode {
            1 => { press(&mut app, KeyCode::Char('w')); app.handle_paste(&edit); }
            2 => { press(&mut app, KeyCode::Char('?')); app.handle_paste(&edit); }
            3 => { press(&mut app, KeyCode::Char('A')); press(&mut app, KeyCode::Char('S')); }
            4 => {
                for item in &mut app.decisions {
                    item.draft = Some(Draft { answer: Answer::Own { text: edit.clone() }, revision: item.revision });
                }
                app.decisions_panel.review = true;
                app.decisions_panel.review_scroll.set(scroll);
            }
            _ => {}
        }
        let (width, height) = size;
        let area = Rect::new(1, 1, width.min(u16::MAX - 2), height.min(u16::MAX - 2));
        let outer = Rect::new(0, 0, area.width + 2, area.height + 2);
        let mut buffer = Buffer::empty(outer);
        for cell in &mut buffer.content { cell.set_symbol("~"); }
        let cursor = crate::widgets::decisions::render(&app, area, &mut buffer);
        if let Some((x, y)) = cursor {
            prop_assert!(area.contains((x, y).into()));
        }
        for y in 0..outer.height {
            for x in [0, outer.width - 1] {
                prop_assert_eq!(buffer[(x, y)].symbol(), "~");
            }
        }
        for x in 0..outer.width {
            for y in [0, outer.height - 1] {
                prop_assert_eq!(buffer[(x, y)].symbol(), "~");
            }
        }
    }

    #[test]
    fn random_focused_details_are_reachable_with_scroll_hints(
        before in 0usize..10,
        after in 0usize..10,
        detail_count in 0usize..40,
        size in prop_oneof![(44u16..110, 6u16..20), (112u16..180, 6u16..20)],
        padding in prop_oneof![Just(""), Just("界"), Just("e\u{301}"), Just("🦀")],
    ) {
        let (width, height) = size;
        let mut app = populated((0..before + after + 1).map(|_| (String::new(), Vec::new(), false)).collect());
        let selected = before;
        app.decisions[selected].question = "FOCUS".into();
        app.decisions[selected].context = Some((0..detail_count).map(|i| format!("{padding} R{i:03}\n")).collect());
        app.decisions[selected].refs.clear();
        app.decisions_panel.selected = Some(ItemKey::Decision(app.decisions[selected].key.clone()));
        let (initial, _) = render(&app, width, height);
        let marker = if width >= 112 { '▸' } else { '▾' };
        prop_assert!(initial.contains(marker), "{width}x{height}\n{initial}");
        let mut seen = vec![false; detail_count];
        let mut reached_end = false;
        for step in 0..detail_count + 32 {
            let (screen, _) = render(&app, width, height);
            for (index, seen) in seen.iter_mut().enumerate() {
                *seen |= screen.contains(&format!("R{index:03}"));
            }
            if step == 0 && detail_count > usize::from(height) && width >= 70 {
                prop_assert!(screen.contains("J/K scroll"), "{width}x{height}\n{screen}");
            }
            let offset = app.decisions_panel.detail_scroll.get();
            press(&mut app, KeyCode::Char('J'));
            render(&app, width, height);
            if app.decisions_panel.detail_scroll.get() == offset {
                reached_end = true;
                break;
            }
        }
        prop_assert!(reached_end);
        prop_assert!(seen.iter().all(|seen| *seen), "missing {:?} at {width}x{height}", seen);
        app.decisions_panel.detail_scroll.set(usize::MAX);
        render(&app, width, height);
        let bottom = app.decisions_panel.detail_scroll.get();
        press(&mut app, KeyCode::Char('J'));
        render(&app, width, height);
        prop_assert_eq!(app.decisions_panel.detail_scroll.get(), bottom);
        press(&mut app, KeyCode::Char('k'));
        if selected > 0 { prop_assert_eq!(app.decisions_panel.detail_scroll.get(), 0); }
    }
}
