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

#[test]
fn a_reply_starts_at_its_first_word() {
    // models open a reply with a blank line or two. keeping them means
    // the byte offsets into the stream don't survive the settle
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("\n\nhello");

    assert_eq!(stream.text, "hello");
}

#[test]
fn a_leading_newline_leaves_the_turn_looking_idle() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("\n");

    assert!(stream.text.is_empty());
}

#[test]
fn a_newline_inside_a_reply_is_kept() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("hello");
    stream.push_text("\n\nworld");

    assert_eq!(stream.text, "hello\n\nworld");
}

#[test]
fn the_typewriter_counts_only_what_it_kept() {
    let mut stream = StreamingState::new();
    stream.start();
    stream.push_text("\n\n\n\nhi");
    for _ in 0..20 {
        stream.advance_typewriter();
    }

    assert_eq!(stream.visible_text(), "hi");
}
