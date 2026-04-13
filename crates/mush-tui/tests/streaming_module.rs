use mush_tui::streaming::StreamingState;

#[test]
fn streaming_module_exposes_typewriter_state() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("hello world");

    assert_eq!(stream.visible_text(), "");

    stream.advance_typewriter();
    assert!(!stream.visible_text().is_empty());

    for _ in 0..20 {
        stream.advance_typewriter();
    }

    assert_eq!(stream.visible_text(), "hello world");
}

#[test]
fn typewriter_handles_incremental_appends() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("hello ");
    stream.advance_typewriter();
    let after_first = stream.visible_text().to_string();
    assert!(!after_first.is_empty());

    stream.push_text("world");
    for _ in 0..20 {
        stream.advance_typewriter();
    }
    assert_eq!(stream.visible_text(), "hello world");
}

#[test]
fn typewriter_handles_unicode() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("café ☕ résumé");
    for _ in 0..20 {
        stream.advance_typewriter();
    }
    assert_eq!(stream.visible_text(), "café ☕ résumé");
}

#[test]
fn typewriter_thinking_tracks_independently() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("output");
    stream.push_thinking("reasoning");
    stream.advance_typewriter();

    assert!(!stream.visible_text().is_empty());
    assert!(!stream.visible_thinking().is_empty());
}
