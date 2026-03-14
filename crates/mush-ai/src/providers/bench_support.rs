pub(crate) fn tool_call_json_fragments(
    chunk_count: usize,
    arg_bytes: usize,
) -> (String, Vec<String>) {
    let payload = "x".repeat(arg_bytes);
    let full_json = format!(r#"{{"payload":"{payload}"}}"#);
    let chunk_count = chunk_count.max(1).min(full_json.len().max(1));
    let base = full_json.len() / chunk_count;
    let remainder = full_json.len() % chunk_count;
    let mut fragments = Vec::with_capacity(chunk_count);
    let mut start = 0;

    for index in 0..chunk_count {
        let len = base + usize::from(index < remainder);
        let end = start + len;
        fragments.push(full_json[start..end].to_string());
        start = end;
    }

    (full_json, fragments)
}
