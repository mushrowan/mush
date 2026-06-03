use mush_ai::wire_log;

#[test]
fn journal_writes_records_to_file() {
    let dir = std::env::temp_dir().join(format!("mush-wire-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("wire.jsonl");
    unsafe {
        std::env::set_var("MUSH_WIRE_LOG", &path);
    }

    let id = wire_log::next_call_id();
    wire_log::log_request(
        id,
        "POST",
        "https://x/v1/messages",
        &[
            ("authorization", "Bearer sk-ant-secret-token-xyz"),
            ("anthropic-version", "2023-06-01"),
        ],
        Some(r#"{"model":"claude","messages":[{"role":"user","content":"hi"}]}"#),
    );
    wire_log::log_sse(
        id,
        0,
        "content_block_delta",
        r#"{"type":"input_json_delta","partial_json":"{\"tool_calls\""}"#,
    );

    let body = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 records, got: {body}");
    let req: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(req["kind"], "request");
    assert_eq!(req["headers"]["authorization"], "<redacted>");
    assert_eq!(req["headers"]["anthropic-version"], "2023-06-01");
    assert_eq!(req["body"]["model"], "claude");
    let sse: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(sse["kind"], "sse");
    assert!(sse["data"].as_str().unwrap().contains("input_json_delta"));
    std::fs::remove_dir_all(&dir).ok();
}
