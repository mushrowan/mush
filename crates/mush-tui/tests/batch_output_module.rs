use mush_tui::batch_output::{BatchSection, parse_batch_output, truncate_output};

#[test]
fn batch_output_module_exposes_parser_and_preview_helpers() {
    let parsed = parse_batch_output(
        "--- [0] read [ok] ---\nhello\n\n--- [1] bash [error] ---\nboom\n\nbatch: 1/2 succeeded, 1 failed",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].content, "hello");
    assert!(parsed[1].is_error);

    let section = BatchSection {
        is_error: false,
        content: "fine".into(),
    };
    assert!(!section.is_error);

    let preview = truncate_output(
        &(0..14)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    assert!(preview.contains("line 0"));
    assert!(preview.contains("2 more lines"));
}
