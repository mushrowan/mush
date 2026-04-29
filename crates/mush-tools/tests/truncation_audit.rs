//! truncation audit: what does the model actually see?
//!
//! the goal of this file is to be the single source of truth for
//! "if a tool produces N lines / M bytes of output, what shows up in
//! the assistant's tool-result message?". every truncation path the
//! model can hit gets exercised here, with the resulting bytes pinned
//! so accidental drift fails noisily.
//!
//! # the contract today
//!
//! - **central truncation** (`mush_agent::truncation::apply`) is the
//!   last stop before the agent loop forwards a [`ToolResult`] to the
//!   model. each tool declares its preferred direction via
//!   [`AgentTool::output_limit`]:
//!     - `Head`  → keep the first `MAX_LINES` / `MAX_BYTES` (ls,
//!       read, repo_map, skills, web_fetch)
//!     - `Tail`  → keep the last `MAX_LINES` / `MAX_BYTES` (bash)
//!     - `Middle`→ keep half from the start, half from the end
//!       (default; batch explicitly opts in)
//!   when central truncation kicks in, the full text is dumped to
//!   `~/.local/share/mush/tool-output/tool_<unix_ms>.txt` and a hint
//!   is folded into the preview pointing the model at that file.
//!
//! - **per-tool semantic truncation** runs *before* central
//!   truncation. it's used when a tool can produce a more useful
//!   summary than blind line-tail (e.g. read advertises `offset=N` to
//!   continue, find/glob/grep say "narrow your search"). these tools
//!   keep their own output well below the central caps so their hint
//!   survives.
//!
//! - **display-only truncation** in `mush-tui` (`truncate_output`,
//!   `… (N more lines)`) only affects what the human sees in the
//!   chat preview. the model never sees it.
//!
//! # invariants every test pins
//!
//! 1. the model is told *something* was truncated (no silent loss)
//! 2. the model is told *how to recover* (file path, offset hint, or
//!    "narrow your search")
//! 3. totals (line count or remaining count) are preserved so the
//!    model can decide whether recovery is worth it
//! 4. the recovery hint always survives extreme truncation
//!    (max_lines = 1) without being mangled

use mush_agent::tool::{AgentTool, OutputLimit, ToolResult};
use mush_agent::truncation::{self, MAX_BYTES, MAX_LINES};
use mush_ai::types::ToolResultContentPart;
use mush_tools::read::ReadTool;
use mush_tools::util;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;

// helpers

fn extract_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|p| match p {
            ToolResultContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn make_lines(n: usize) -> String {
    (0..n)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// mask the non-deterministic `tool_<unix_ms>.txt` filename and the
/// data-dir prefix so byte snapshots stay stable across test runs
fn mask_saved_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("Full output: ") {
        out.push_str(&rest[..start]);
        out.push_str("Full output: <DATA_DIR>/tool-output/tool_<TS>.txt");
        rest = &rest[start + "Full output: ".len()..];
        // skip past the path: ends at first ']' or whitespace boundary
        let end = rest
            .find(|c: char| c == ']' || c == '\n' || c == ' ')
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// run a closure with `MUSH_DATA_DIR` pinned to a tempdir so saved
/// output goes somewhere deterministic and isolated per-test
fn with_data_dir<R>(f: impl FnOnce(&Path) -> R) -> R {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: nextest runs each test in its own process, so mutating
    // env here can't race with sibling tests
    unsafe {
        std::env::set_var("MUSH_DATA_DIR", tmp.path());
    }
    f(tmp.path())
}

// section: central truncation (the hint contract)

#[test]
fn audit_central_head_truncation() {
    // 5000 lines through Head truncation: hint sits at the TOP of the
    // preview so the model sees the recovery action before reading
    // 2000 lines of output. preview body starts at line 0
    let big = make_lines(5000);
    let out = with_data_dir(|_| truncation::apply(ToolResult::text(big), OutputLimit::Head));
    let text = mask_saved_path(&extract_text(&out));

    // structural pins
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
        text.contains("Use the Grep tool to search or the Read tool with offset/limit"),
        "recovery action present"
    );
    assert!(
        text.contains("Full output: <DATA_DIR>/tool-output/tool_<TS>.txt"),
        "saved path present and deterministic"
    );
    assert!(
        text.contains("\n\nline 0\nline 1\n"),
        "head preview body starts at line 0"
    );
    // size sanity
    assert!(text.len() <= MAX_BYTES + 512, "preview within budget");
}

#[test]
fn audit_central_tail_truncation() {
    // 5000 lines through Tail (bash): hint comes FIRST, preview ends
    // with the last lines. agents reading bash output care most about
    // the end (errors, final results)
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
fn audit_central_middle_truncation() {
    // 5000 lines through Middle: hint at the top, then head + a `[…]`
    // gap marker + tail. the gap marker tells the model where the
    // omitted span lives even though the count is in the top hint
    let big = make_lines(5000);
    let out = with_data_dir(|_| truncation::apply(ToolResult::text(big), OutputLimit::Middle));
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
        "saved path present"
    );
    assert!(text.contains("\n\nline 0\n"), "head preview kept");
    assert!(
        text.contains("\n\n[…]\n\n"),
        "gap marker between head and tail"
    );
    assert!(text.ends_with("line 4999"), "tail preview kept");
}

// section: extreme cases

#[test]
fn audit_central_byte_overflow_truncation() {
    // 1 huge line that exceeds MAX_BYTES on its own. byte-overflow must
    // still emit the recovery hint AND a sampled head/tail of the line
    // so the model can classify the content (base64? minified js?)
    let huge_line = "x".repeat(MAX_BYTES + 10000);
    let out =
        with_data_dir(|_| truncation::apply(ToolResult::text(huge_line), OutputLimit::Middle));
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
    // budget of 1 line, regardless of direction: the hint still has
    // every actionable phrase the model needs
    let big = make_lines(5000);
    for direction in [OutputLimit::Head, OutputLimit::Tail, OutputLimit::Middle] {
        let out = with_data_dir(|_| {
            truncation::truncate(ToolResult::text(big.clone()), 1, usize::MAX, direction)
        });
        let text = mask_saved_path(&extract_text(&out));
        for fragment in [
            "Use the Grep tool to search",
            "the Read tool with offset/limit",
            "Do not use bash cat",
            "Full output: <DATA_DIR>/tool-output/tool_<TS>.txt",
        ] {
            assert!(
                text.contains(fragment),
                "{direction:?} extreme budget dropped fragment {fragment:?}\n--- got ---\n{text}"
            );
        }
    }
}

// section: per-tool semantic truncation

#[tokio::test]
async fn audit_read_tool_truncates_long_file_with_offset_hint() {
    // a 5000-line file read through the read tool: model sees the
    // first ~MAX_LINES lines with an `offset=` hint pointing at the
    // exact line to resume from. importantly the read tool's own hint
    // must fit *under* the central truncation cap, otherwise the
    // central pass would obliterate it
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.txt");
    let body = make_lines(5000);
    std::fs::write(&file, &body).unwrap();

    let tool = ReadTool::new(Arc::from(dir.path().to_path_buf().into_boxed_path()));
    let raw = tool
        .execute(serde_json::json!({ "file_path": file.to_string_lossy() }))
        .await;
    // route through the central pass exactly like the agent loop does
    let routed = truncation::apply(raw, tool.output_limit());
    let text = extract_text(&routed);

    assert!(text.starts_with("L1: line 0"), "read uses 1-indexed labels");
    assert!(
        text.contains("more lines in file. Use offset="),
        "read hint"
    );
    // central pass should not have stomped on the read hint
    assert!(
        !text.contains("Use the Grep tool to search or the Read tool"),
        "central truncation should not fire on a tidy read output:\n{text}"
    );
    // read advertises a concrete next offset
    let next_offset_hint = text.lines().last().unwrap_or("");
    assert!(
        next_offset_hint.contains("Use offset=") && next_offset_hint.contains("to continue"),
        "last line should be the actionable hint, got {next_offset_hint:?}"
    );
}

#[test]
fn audit_truncate_lines_helper_for_find_grep_glob() {
    // util::truncate_lines is what find/glob/grep-like list outputs
    // go through. cap is 200 results, hint says "narrow your search".
    // there's no saved file path here: dropped results are *gone*
    // from the model's view, the only recovery is a tighter pattern.
    // this test exists to flag if anyone tries to rip the hint out
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
    // small input keeps the count header so the model can spot
    // "exactly N matches" without re-counting the buffer itself
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

// section: max-byte / max-line constants

#[test]
fn audit_central_caps_match_documented_defaults() {
    // pi-mono, opencode, codex all settled on ~2000 lines / ~50KB.
    // pinning here so anyone bumping these has to update the audit
    assert_eq!(MAX_LINES, 2000);
    assert_eq!(MAX_BYTES, 50 * 1024);
}
