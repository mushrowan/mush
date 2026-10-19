use mush_tui::batch_output::parse_batch_output;

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
