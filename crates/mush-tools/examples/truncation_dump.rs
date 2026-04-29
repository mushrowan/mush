//! ad-hoc dumper: prints what the model sees for each truncation path
use mush_agent::tool::{AgentTool, OutputLimit, ToolResult};
use mush_agent::truncation;
use mush_ai::types::ToolResultContentPart;
use mush_tools::read::ReadTool;
use mush_tools::util;
use std::sync::Arc;

fn extract(r: &ToolResult) -> String {
    r.content
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

fn first_n_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}
fn last_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn banner(name: &str) {
    println!("\n========== {name} ==========");
}

#[tokio::main]
async fn main() {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MUSH_DATA_DIR", tmp.path());
    }

    banner("CENTRAL HEAD (5000 lines, ls/read/web_fetch path)");
    let big = make_lines(5000);
    let out = truncation::apply(ToolResult::text(big.clone()), OutputLimit::Head);
    let text = extract(&out);
    println!(
        "[first 4 lines]\n{}\n[last 4 lines]\n{}",
        first_n_lines(&text, 4),
        last_n_lines(&text, 4)
    );
    println!("(total chars: {})", text.len());

    banner("CENTRAL TAIL (5000 lines, bash path)");
    let out = truncation::apply(ToolResult::text(big.clone()), OutputLimit::Tail);
    let text = extract(&out);
    println!(
        "[first 4 lines]\n{}\n[last 4 lines]\n{}",
        first_n_lines(&text, 4),
        last_n_lines(&text, 4)
    );
    println!("(total chars: {})", text.len());

    banner("CENTRAL MIDDLE (5000 lines, batch / default path)");
    let out = truncation::apply(ToolResult::text(big.clone()), OutputLimit::Middle);
    let text = extract(&out);
    println!(
        "[first 4 lines]\n{}\n[last 4 lines]\n{}",
        first_n_lines(&text, 4),
        last_n_lines(&text, 4)
    );
    println!("(total chars: {})", text.len());

    banner("READ TOOL on 5000-line file");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.txt");
    std::fs::write(&file, &big).unwrap();
    let tool = ReadTool::new(Arc::from(dir.path().to_path_buf().into_boxed_path()));
    let raw = tool
        .execute(serde_json::json!({"file_path": file.to_string_lossy()}))
        .await;
    let routed = truncation::apply(raw, tool.output_limit());
    let text = extract(&routed);
    println!(
        "[first 3 lines]\n{}\n[last 3 lines]\n{}",
        first_n_lines(&text, 3),
        last_n_lines(&text, 3)
    );

    banner("util::truncate_lines (find/glob/grep helper) on 500 results");
    let lines: Vec<String> = (0..500).map(|i| format!("file_{i}.rs")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let out = util::truncate_lines(&refs, "matches");
    println!(
        "[first 3 lines]\n{}\n[last 3 lines]\n{}",
        first_n_lines(&out, 3),
        last_n_lines(&out, 3)
    );

    banner("BYTE OVERFLOW (single huge line through Middle)");
    let huge = "x".repeat(100_000);
    let out = truncation::apply(ToolResult::text(huge), OutputLimit::Middle);
    let text = extract(&out);
    let preview = if text.len() > 300 {
        format!("{}\n...\n{}", &text[..150], &text[text.len() - 150..])
    } else {
        text.clone()
    };
    println!("{preview}\n(total chars: {})", text.len());
}
