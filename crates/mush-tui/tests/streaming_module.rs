use mush_tui::streaming::StreamingState;

#[test]
fn streaming_module_exposes_typewriter_state() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.text.push_str("hello world");

    assert_eq!(stream.visible_text(), "");

    stream.advance_typewriter();
    assert!(!stream.visible_text().is_empty());

    for _ in 0..20 {
        stream.advance_typewriter();
    }

    assert_eq!(stream.visible_text(), "hello world");
}
