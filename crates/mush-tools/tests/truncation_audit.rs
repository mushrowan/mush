use mush_agent::tool::{AgentTool, OutputLimit, ToolResult};
use mush_agent::truncation::{self, MAX_BYTES, MAX_LINES};
use mush_tools::read::ReadTool;
use mush_tools::util::{self, extract_text};
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;

fn make_lines(n: usize) -> String {
    (0..n)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn mask_saved_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("Full output: ") {
        out.push_str(&rest[..start]);
        out.push_str("Full output: <DATA_DIR>/tool-output/tool_<TS>.txt");
        rest = &rest[start + "Full output: ".len()..];
        let end = rest.find([']', '\n', ' ']).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[allow(
    clippy::unwrap_used,
    reason = "test helper, panic on tmpdir failure is fine"
)]
fn with_data_dir<R>(f: impl FnOnce(&Path) -> R) -> R {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: nextest runs each test in its own process, so mutating
    unsafe {
        std::env::set_var("MUSH_DATA_DIR", tmp.path());
    }
    f(tmp.path())
}

#[test]
fn audit_per_tool_output_limit_choices() {
    use mush_agent::tool::AgentTool;
    use std::sync::Arc;

    let cwd: Arc<std::path::Path> = Arc::from(std::env::temp_dir().as_path());
    let http = mush_ai::http::client();

    let cases: Vec<(&str, OutputLimit, Box<dyn AgentTool>)> = vec![
        (
            "read",
            OutputLimit::Head,
            Box::new(mush_tools::read::ReadTool::new(cwd.clone())),
        ),
        (
            "edit",
            OutputLimit::Head,
            Box::new(mush_tools::edit::EditTool::new(cwd.clone())),
        ),
        (
            "write",
            OutputLimit::Head,
            Box::new(mush_tools::write::WriteTool::new(cwd.clone())),
        ),
        (
            "apply_patch",
            OutputLimit::Head,
            Box::new(mush_tools::apply_patch::ApplyPatchTool::new(cwd.clone())),
        ),
        (
            "web_search",
            OutputLimit::Head,
            Box::new(mush_tools::web_search::WebSearchTool::new(http)),
        ),
    ];

    for (name, expected, tool) in cases {
        assert_eq!(
            tool.output_limit(),
            expected,
            "tool {name} declares the wrong output_limit"
        );
    }
}

#[test]
fn audit_central_head_truncation() {
    let big = make_lines(5000);
    let out = with_data_dir(|_| truncation::apply(ToolResult::text(big), OutputLimit::Head));
    let text = mask_saved_path(&extract_text(&out));

    assert!(
        text.starts_with('['),
        "hint at top, got prefix {:?}",
        &text[..text.len().min(80)]
    );
    assert!(
        text.contains("lines truncated (5000 total)"),
        "total preserved"
    );
    assert!(
        text.contains("Full output: <DATA_DIR>/tool-output/tool_<TS>.txt"),
        "saved path present and deterministic"
    );
    assert!(
        text.contains("\n\nline 0\nline 1\n"),
        "head preview body starts at line 0"
    );
    assert!(text.len() <= MAX_BYTES + 512, "preview within budget");
}

#[test]
fn audit_central_tail_truncation() {
    let big = make_lines(5000);
    let out = with_data_dir(|_| truncation::apply(ToolResult::text(big), OutputLimit::Tail));
    let text = mask_saved_path(&extract_text(&out));

    assert!(
        text.starts_with('['),
        "tail truncation puts hint at top, got: {:?}",
        &text[..text.len().min(80)]
    );
    assert!(
        text.contains("lines truncated… (5000 total)"),
        "total preserved"
    );
    assert!(
        text.contains("Full output: <DATA_DIR>/tool-output/tool_<TS>.txt"),
        "saved path present"
    );
    assert!(text.ends_with("line 4999"), "tail kept last line");
}

#[test]
fn audit_central_byte_overflow_truncation() {
    let huge_line = "x".repeat(MAX_BYTES + 10000);
    let out = with_data_dir(|_| truncation::apply(ToolResult::text(huge_line), OutputLimit::Head));
    let text = mask_saved_path(&extract_text(&out));
    assert!(text.contains("lines truncated"), "byte-overflow truncated");
    assert!(
        text.contains("Full output: <DATA_DIR>/tool-output/tool_<TS>.txt"),
        "byte-overflow saves full output too"
    );
    assert!(
        text.contains("[partial line]"),
        "byte-overflow marks partial line"
    );
    assert!(
        text.contains("xxxx"),
        "byte-overflow includes a sample of the line content"
    );
}

#[test]
fn audit_central_extreme_budget_keeps_hint_intact() {
    let big = make_lines(5000);
    for direction in [OutputLimit::Head, OutputLimit::Tail] {
        let out = with_data_dir(|_| {
            truncation::truncate(ToolResult::text(big.clone()), 1, usize::MAX, direction)
        });
        let text = mask_saved_path(&extract_text(&out));
        let fragment = "Full output: <DATA_DIR>/tool-output/tool_<TS>.txt";
        assert!(
            text.contains(fragment),
            "{direction:?} extreme budget dropped fragment {fragment:?}\n--- got ---\n{text}"
        );
    }
}

#[tokio::test]
async fn audit_read_tool_truncates_long_file_with_offset_hint() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.txt");
    let body = make_lines(5000);
    std::fs::write(&file, &body).unwrap();

    let tool = ReadTool::new(Arc::from(dir.path().to_path_buf().into_boxed_path()));
    let raw = tool
        .execute(serde_json::json!({ "file_path": file.to_string_lossy() }))
        .await;
    let routed = truncation::apply(raw, tool.output_limit());
    let text = extract_text(&routed);

    assert!(text.starts_with("L1: line 0"), "read uses 1-indexed labels");
    assert!(
        text.contains("more lines in file. Use offset="),
        "read hint"
    );
    assert!(
        !text.contains("Use the Grep tool to search or the Read tool"),
        "central truncation should not fire on a tidy read output:\n{text}"
    );
    let next_offset_hint = text.lines().last().unwrap_or("");
    assert!(
        next_offset_hint.contains("Use offset=") && next_offset_hint.contains("to continue"),
        "last line should be the actionable hint, got {next_offset_hint:?}"
    );
}

#[test]
fn audit_truncate_lines_helper_for_find_grep_glob() {
    let lines: Vec<String> = (0..500).map(|i| format!("file_{i}.rs")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let out = util::truncate_lines(&refs, "matches");

    let preview = out.lines().take(3).collect::<Vec<_>>().join("\n");
    assert!(preview.contains("file_0.rs"));
    assert!(out.contains("[300 more matches. narrow your search.]"));
    assert!(
        !out.contains("file_499.rs"),
        "truncated results must not leak through"
    );
}

#[test]
fn audit_truncate_lines_under_cap_includes_count_header() {
    let lines: Vec<String> = (0..3).map(|i| format!("hit {i}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let out = util::truncate_lines(&refs, "matches");
    assert_eq!(out, "3\n\nhit 0\nhit 1\nhit 2");
}

#[test]
fn audit_truncate_lines_empty_says_no_results() {
    let out = util::truncate_lines(&[], "matches");
    assert_eq!(out, "no matches found");
}

#[test]
fn audit_central_caps_match_documented_defaults() {
    assert_eq!(MAX_LINES, 2000);
    assert_eq!(MAX_BYTES, 50 * 1024);
}
