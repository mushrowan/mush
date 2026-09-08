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
    assert!(preview.contains("line 13"));
    assert!(preview.contains("9 earlier lines"));
}

#[test]
fn a_sub_tool_output_that_looks_like_batch_markup_stays_in_its_section() {
    let parsed = parse_batch_output(concat!(
        "--- [0] grep [ok] ---\n",
        "batch.rs:225: let header = format!(\"--- [{i}] {tool_name} [{status}] ---\\n\");\n",
        "--- [7] x [ok] ---\n",
        "batch: something the file says\n",
        "\n",
        "--- [1] read [ok] ---\n",
        "fn main() {}\n",
        "\n",
        "batch: 2/2 succeeded, 0 failed",
    ));
    assert_eq!(parsed.len(), 2);
    assert!(parsed[0].content.contains("--- [7] x [ok] ---"));
    assert!(parsed[0].content.contains("batch: something the file says"));
    assert_eq!(parsed[1].content, "fn main() {}");
}
