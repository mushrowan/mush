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
        state in 0u8..5,
    ) {
        let mut app = populated(questions);
        for (index, item) in app.decisions.iter_mut().enumerate() {
            item.asker.name = item.question.clone();
            item.key.id = format!("{index}{}", item.question);
            item.lean = (!item.options.is_empty()).then_some(0);
            item.clarified = Some(edit.clone());
            match state {
                0 => item.status = DecisionStatus::Clarifying { question: edit.clone(), at: Timestamp::zero() },
                1 => item.assume_at = Some(Timestamp::from_ms(1)),
                2 => item.draft = Some(Draft { answer: Answer::Own { text: edit.clone() }, revision: 0 }),
                3 => item.draft = Some(Draft { answer: Answer::Later, revision: item.revision }),
                _ => {}
            }
        }
        app.children = confirmations.iter().enumerate().map(|(index, text)| child(&format!("c{index}"), Some(text))).collect();
        for child in &mut app.children {
            app.decisions_panel.confirms.insert(child.id.clone(), state % 2 == 0);
            if state == 1 { child.status = mush_runtime::ChildStatus::Deleted; }
            if state == 2 { child.status = mush_runtime::ChildStatus::Idle; }
        }
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
    fn random_panel_navigation_and_editor_actions_preserve_state(
        content in text(),
        actions in proptest::collection::vec(0u8..20, 1..64),
        option in 0usize..3,
        width in 44u16..180,
    ) {
        let mut app = app();
        open(&mut app);
        let keys = flat(&groups(&app.decisions, &app.children));
        for code in [KeyCode::Tab, KeyCode::BackTab, KeyCode::End, KeyCode::Home,
                     KeyCode::Down, KeyCode::Up, KeyCode::PageDown, KeyCode::PageUp] {
            press(&mut app, code);
            prop_assert!(keys.contains(app.decisions_panel.selected.as_ref().unwrap()));
        }
        select(&mut app, "layout");
        press(&mut app, KeyCode::Char(char::from(b'a' + option as u8)));
        press(&mut app, KeyCode::Char('w'));
        app.handle_paste(&content);
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Home, KeyCode::End] {
            press(&mut app, code);
            let editor = app.decisions_panel.editor.as_ref().unwrap();
            prop_assert!(editor.text.is_char_boundary(editor.cursor));
        }
        render(&app, width, 16);
        ctrl(&mut app, 'u');
        prop_assert!(app.decisions_panel.editor.as_ref().unwrap().text.is_empty());
        app.handle_paste(&content);
        press(&mut app, KeyCode::Enter);
        ctrl(&mut app, 'n');
        ctrl(&mut app, 'p');
        app.children = vec![child("models", Some("confirm"))];
        app.decisions_panel.selected = Some(ItemKey::Confirm("sub-models".into()));
        press(&mut app, KeyCode::Char('a'));
        render(&app, width, 16);
        for action in actions {
            let code = match action {
                0 => KeyCode::Tab, 1 => KeyCode::BackTab,
                2 => KeyCode::Home, 3 => KeyCode::End,
                4 => KeyCode::Left, 5 => KeyCode::Right,
                6 => KeyCode::Backspace, 7 => KeyCode::Enter,
                8 => KeyCode::Char('w'), 9 => KeyCode::Char('?'),
                10 => KeyCode::Char('x'), 11 => KeyCode::Char('a'),
                12 => KeyCode::Char('l'), 13 => KeyCode::Char('S'),
                14 => KeyCode::Char('j'), 15 => KeyCode::Char('k'),
                16 => KeyCode::PageDown, 17 => KeyCode::PageUp,
                18 => KeyCode::Char('A'), _ => KeyCode::Esc,
            };
            if !app.decisions_panel.open { open(&mut app); }
            press(&mut app, code);
            if let Some(editor) = &app.decisions_panel.editor {
                prop_assert!(editor.cursor <= editor.text.len());
                prop_assert!(editor.text.is_char_boundary(editor.cursor));
            }
            render(&app, width, 16);
        }
        open(&mut app);
        app.note_decisions(Vec::new());
        prop_assert!(!app.decisions_panel.review || !app.children.is_empty());
        app.children.clear();
        app.decisions_panel.review = true;
        app.note_decisions(Vec::new());
        prop_assert!(!app.decisions_panel.review);
        app.decisions_panel.open = false;
        prop_assert!(!paste(&mut app, &content));
        prop_assert!(handle_key(&mut app, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn random_focused_details_are_reachable_with_scroll_hints(
        before in 0usize..10,
        after in 0usize..10,
        detail_count in 0usize..16,
        option_count in 1usize..5,
        half_pages in any::<bool>(),
        size in prop_oneof![(44u16..110, 6u16..20), (112u16..180, 6u16..20)],
        padding in prop_oneof![Just(""), Just("界"), Just("e\u{301}"), Just("🦀")],
    ) {
        let (width, height) = size;
        let mut app = populated((0..before + after + 1).map(|_| (String::new(), Vec::new(), false)).collect());
        let selected = before;
        app.decisions[selected].question = "FOCUS".into();
        let mut tokens = Vec::new();
        let mut field = |tag: &str, count: usize| {
            (0..count).map(|index| {
                let token = format!("{tag}{index:02}");
                tokens.push(token.clone());
                format!("{padding} {token}\n")
            }).collect::<String>()
        };
        let focused = &mut app.decisions[selected];
        focused.context = Some(field("CTX", detail_count));
        focused.refs = vec![field("REF", detail_count)];
        focused.options = (0..option_count).map(|index| DecisionOption {
            label: field(&format!("L{index}"), detail_count.max(1)),
            detail: Some(field(&format!("D{index}"), detail_count)),
            pros: vec![field(&format!("P{index}"), detail_count)],
            cons: vec![field(&format!("C{index}"), detail_count)],
        }).collect();
        app.decisions_panel.selected = Some(ItemKey::Decision(app.decisions[selected].key.clone()));
        let (initial, _) = render(&app, width, height);
        let marker = if width >= 112 { '▸' } else { '▾' };
        prop_assert!(initial.contains(marker), "{width}x{height}\n{initial}");
        let mut seen = vec![false; tokens.len()];
        let down = if half_pages {
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
        } else {
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT)
        };
        let up = if half_pages {
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
        } else {
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)
        };
        let budget = tokens.len() * 4 + 64;
        for key in [down, up] {
            let mut reached_end = false;
            for step in 0..budget {
                let (screen, _) = render(&app, width, height);
                for (token, seen) in tokens.iter().zip(&mut seen) {
                    *seen |= screen.contains(token);
                }
                if step == 0 && detail_count > usize::from(height) && width >= 70 {
                    prop_assert!(screen.contains("J/K scroll"), "{width}x{height}\n{screen}");
                }
                let offset = app.decisions_panel.detail_scroll.get();
                crate::input::handle_key(&mut app, key);
                render(&app, width, height);
                if app.decisions_panel.detail_scroll.get() == offset {
                    reached_end = true;
                    break;
                }
            }
            prop_assert!(reached_end);
            let missing = tokens.iter().zip(&seen).filter(|(_, seen)| !**seen).map(|(token, _)| token).collect::<Vec<_>>();
            prop_assert!(missing.is_empty(), "missing {missing:?} at {width}x{height}");
            seen.fill(false);
        }
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
