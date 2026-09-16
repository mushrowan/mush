use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use crate::util::resolve_path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    #[serde(alias = "file_path")]
    path: String,
    #[serde(rename = "oldText", alias = "old_string")]
    old_text: Option<String>,
    #[serde(rename = "newText", alias = "new_string")]
    new_text: Option<String>,
    #[serde(default, deserialize_with = "mush_agent::de::json_seq")]
    edits: Vec<Replacement>,
    #[serde(
        default,
        alias = "expected_replacements",
        deserialize_with = "mush_agent::de::opt_uint"
    )]
    expected_matches: Option<usize>,
    #[serde(default)]
    replace_all: bool,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    start_line: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    end_line: Option<usize>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    #[serde(rename = "oldText", alias = "old_string")]
    pub old_text: String,
    #[serde(rename = "newText", alias = "new_string")]
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Clone, Copy)]
enum Matches {
    Exactly(usize),
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
enum EditArgsError {
    #[error("end_line requires start_line")]
    EndLineRequiresStartLine,
    #[error("end_line ({end_line}) must be >= start_line ({start_line})")]
    EndBeforeStart { start_line: usize, end_line: usize },
    #[error("provide oldText and newText, or a non-empty edits array")]
    NoReplacements,
    #[error("start_line/end_line apply to a single oldText, not to edits")]
    RangeWithBatch,
}

impl EditArgs {
    fn into_opts(self) -> Result<(String, Vec<Replacement>, EditOpts), EditArgsError> {
        if self.end_line.is_some() && self.start_line.is_none() {
            return Err(EditArgsError::EndLineRequiresStartLine);
        }
        if let (Some(start_line), Some(end_line)) = (self.start_line, self.end_line)
            && end_line < start_line
        {
            return Err(EditArgsError::EndBeforeStart {
                start_line,
                end_line,
            });
        }

        let mut edits = self.edits;
        if let (Some(old_text), Some(new_text)) = (self.old_text, self.new_text) {
            edits.push(Replacement {
                old_text,
                new_text,
                replace_all: false,
            });
        }
        if self.replace_all {
            for edit in &mut edits {
                edit.replace_all = true;
            }
        }
        if edits.is_empty() {
            return Err(EditArgsError::NoReplacements);
        }
        if edits.len() > 1 && self.start_line.is_some() {
            return Err(EditArgsError::RangeWithBatch);
        }

        Ok((
            self.path,
            edits,
            EditOpts {
                matches: Matches::Exactly(self.expected_matches.unwrap_or(1)),
                start_line: self.start_line,
                end_line: self.end_line,
            },
        ))
    }
}

pub struct EditTool {
    cwd: Arc<Path>,
    ledger: crate::read_ledger::ReadLedger,
}

impl EditTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            cwd,
            ledger: crate::read_ledger::ReadLedger::shared(),
        }
    }

    #[must_use]
    pub fn with_read_ledger(mut self, ledger: crate::read_ledger::ReadLedger) -> Self {
        self.ledger = ledger;
        self
    }
}

#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn label(&self) -> &str {
        "Edit"
    }
    fn description(&self) -> &str {
        "Edit a file by replacing exact text. The oldText must match exactly (including whitespace). \
         Use this for precise, surgical edits. By default oldText must appear exactly once in the \
         file - set replace_all to take every occurrence, or expected_matches when you know the \
         count. Use start_line/end_line to restrict the search to a specific line range. \
         To change several separate places in one file, send one call with an edits array rather \
         than one call per change. Each edits[] entry is matched against the file as it was, so the \
         entries must not overlap, and nothing is written unless every entry resolves."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "path to the file to edit (relative or absolute)"
                },
                "oldText": {
                    "type": "string",
                    "description": "exact text to find and replace (must match exactly)"
                },
                "newText": {
                    "type": "string",
                    "description": "new text to replace the old text with"
                },
                "edits": {
                    "type": "array",
                    "description": "several disjoint replacements in one call, instead of oldText/newText. each is matched against the file as it was, so they must not overlap. prefer this over one call per change",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "exact text to find, must be unique in the file"
                            },
                            "newText": {
                                "type": "string",
                                "description": "new text to replace it with"
                            },
                            "replace_all": {
                                "type": "boolean",
                                "description": "replace every occurrence of this entry's oldText (default false, which requires exactly one)"
                            }
                        },
                        "required": ["oldText", "newText"]
                    }
                },
                "expected_matches": {
                    "type": "integer",
                    "description": "expected number of matches (default 1). edit proceeds only when actual count equals this"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "replace every occurrence, however many. use expected_matches instead when you know the count"
                },
                "start_line": {
                    "type": "integer",
                    "description": "restrict search to lines starting from this line (1-indexed)"
                },
                "end_line": {
                    "type": "integer",
                    "description": "restrict search to lines ending at this line (1-indexed, inclusive). requires start_line"
                }
            },
            "required": ["path"]
        })
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<EditArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let (path, edits, opts) = match args.into_opts() {
            Ok(args) => args,
            Err(error) => return ToolResult::error(error.to_string()),
        };

        let path = resolve_path(&self.cwd, &path);
        let ledger = self.ledger.clone();

        tokio::task::spawn_blocking(move || {
            if let Err(stale) = ledger.check(&path) {
                return ToolResult::error(stale.to_string());
            }
            let result = edit_file_batch(&path, &edits, &opts);
            if result.outcome.is_success() {
                ledger.record(&path);
            }
            result
        })
        .await
        .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
    }
}

#[derive(Clone, Copy)]
struct EditOpts {
    matches: Matches,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

impl Default for EditOpts {
    fn default() -> Self {
        Self {
            matches: Matches::Exactly(1),
            start_line: None,
            end_line: None,
        }
    }
}

fn match_candidates(content: &str, old_text: &str, new_text: &str) -> Vec<(String, String)> {
    let mut candidates: Vec<(String, String)> = vec![(old_text.to_string(), new_text.to_string())];

    if content.contains("\r\n") && old_text.contains('\n') && !old_text.contains("\r\n") {
        candidates.push((
            old_text.replace("\n", "\r\n"),
            new_text.replace("\n", "\r\n"),
        ));
    }

    if !content.contains("\r\n") && old_text.contains("\r\n") {
        candidates.push((
            old_text.replace("\r\n", "\n"),
            new_text.replace("\r\n", "\n"),
        ));
    }

    candidates.dedup();

    let mut extra = Vec::new();
    for (o, n) in &candidates {
        if o.ends_with('\n') && !content.contains(o.as_str()) {
            let trimmed = o.trim_end_matches('\n').trim_end_matches('\r');
            extra.push((trimmed.to_string(), n.clone()));
        } else if !o.ends_with('\n') {
            extra.push((format!("{o}\n"), format!("{n}\n")));
            if content.contains("\r\n") {
                extra.push((format!("{o}\r\n"), format!("{n}\r\n")));
            }
        }
    }
    candidates.extend(extra);

    for actual_old in find_line_trimmed_matches(content, old_text) {
        if actual_old != old_text {
            let adapted_new = if actual_old.contains("\r\n") && !new_text.contains("\r\n") {
                new_text.replace('\n', "\r\n")
            } else {
                new_text.to_string()
            };
            candidates.push((actual_old, adapted_new));
        }
    }

    for actual_old in find_folded_matches(content, old_text) {
        candidates.push((actual_old, new_text.to_string()));
    }

    candidates.dedup();
    candidates
}

fn edit_file_batch(path: &Path, edits: &[Replacement], opts: &EditOpts) -> ToolResult {
    if let [only] = edits {
        let matches = if only.replace_all {
            Matches::All
        } else {
            opts.matches
        };
        return edit_file(
            path,
            &only.old_text,
            &only.new_text,
            &EditOpts { matches, ..*opts },
        );
    }

    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return ToolResult::error(format!("failed to read file: {e}")),
    };

    let mut splices: Vec<(usize, usize, String)> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return ToolResult::error(format!("edits[{i}].oldText cannot be empty"));
        }
        let resolved = match_candidates(&content, &edit.old_text, &edit.new_text)
            .into_iter()
            .find_map(|(old, new)| {
                let starts: Vec<usize> = content
                    .match_indices(old.as_str())
                    .map(|(start, _)| start)
                    .collect();
                let enough = if edit.replace_all {
                    !starts.is_empty()
                } else {
                    starts.len() == 1
                };
                enough.then_some((starts, old, new))
            });
        let Some((starts, matched, replacement)) = resolved else {
            let count = content.matches(edit.old_text.as_str()).count();
            return ToolResult::error(format!(
                "edits[{i}] matched {count} times in {}, expected exactly 1\n\
                 hint: add surrounding context to make oldText unique, or set replace_all",
                path.display()
            ));
        };
        for start in starts {
            splices.push((start, start + matched.len(), replacement.clone()));
        }
    }

    splices.sort_by_key(|(start, _, _)| *start);
    for pair in splices.windows(2) {
        if pair[0].1 > pair[1].0 {
            return ToolResult::error(
                "edits overlap. merge the overlapping entries into one".to_string(),
            );
        }
    }

    let mut updated = content.clone();
    for (start, end, replacement) in splices.iter().rev() {
        updated.replace_range(start..end, replacement);
    }

    if let Err(e) = mush_ai::private_io::write_atomic(path, updated.as_bytes()) {
        return ToolResult::error(format!("failed to write file: {e}"));
    }

    ToolResult::text(format!(
        "applied {} edits to {}\n\ndiff:\n{}",
        edits.len(),
        path.display(),
        format_edit_diff(&content, &updated)
    ))
}

fn edit_file(path: &Path, old_text: &str, new_text: &str, opts: &EditOpts) -> ToolResult {
    if !path.exists() {
        return ToolResult::error(format!("file not found: {}", path.display()));
    }
    if old_text.is_empty() {
        return ToolResult::error("old text cannot be empty");
    }
    if old_text == new_text {
        return ToolResult::error("old text and new text are identical");
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return ToolResult::error(format!("failed to read file: {e}")),
    };

    let has_bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let text_bytes = if has_bom { &bytes[3..] } else { &bytes[..] };
    let content = match std::str::from_utf8(text_bytes) {
        Ok(s) => s,
        Err(e) => return ToolResult::error(format!("failed to read file as utf-8 text: {e}")),
    };

    let (search_content, region_start_byte) = if let Some(start) = opts.start_line {
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let start_idx = start.saturating_sub(1).min(lines.len());
        let end_idx = opts.end_line.unwrap_or(lines.len()).min(lines.len());
        let prefix_bytes: usize = lines[..start_idx].iter().map(|l| l.len()).sum();
        let region: String = lines[start_idx..end_idx].concat();
        (region, prefix_bytes)
    } else {
        (content.to_string(), 0)
    };

    let range_label = if opts.start_line.is_some() {
        format!(
            " (lines {}-{})",
            opts.start_line.unwrap_or(1),
            opts.end_line.unwrap_or_else(|| content.lines().count())
        )
    } else {
        String::new()
    };

    let candidates = match_candidates(&search_content, old_text, new_text);

    let mut mismatch_count: Option<usize> = None;

    for (match_old, match_new) in candidates {
        let count = search_content.matches(&match_old).count();
        if count == 0 {
            continue;
        }
        if let Matches::Exactly(want) = opts.matches
            && count != want
        {
            mismatch_count = Some(count);
            continue;
        }

        let replaced_region = search_content.replace(&match_old, &match_new);

        let replaced = if opts.start_line.is_some() {
            let mut full = String::with_capacity(content.len() + replaced_region.len());
            full.push_str(&content[..region_start_byte]);
            full.push_str(&replaced_region);
            full.push_str(&content[region_start_byte + search_content.len()..]);
            full
        } else {
            replaced_region
        };

        let write_result = if has_bom {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(replaced.as_bytes());
            mush_ai::private_io::write_atomic(path, out)
        } else {
            mush_ai::private_io::write_atomic(path, replaced.as_bytes())
        };

        return match write_result {
            Ok(()) => {
                let diff = format_edit_diff(old_text, new_text);
                let count_note = if count > 1 {
                    format!(" ({count} replacements)")
                } else {
                    String::new()
                };
                let mut text = format!("edited {}{count_note}\n{diff}", path.display());
                if let Some(hint) = crate::util::gitignore_hint(path) {
                    text.push_str("\n\n");
                    text.push_str(&hint);
                }
                ToolResult::text(text)
            }
            Err(e) => ToolResult::error(format!("failed to write file: {e}")),
        };
    }

    if let (Some(found), Matches::Exactly(want)) = (mismatch_count, opts.matches) {
        let first_line = old_text.lines().next().unwrap_or(old_text);
        let match_lines: Vec<usize> = content
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(first_line.trim()))
            .map(|(i, _)| i + 1)
            .collect();
        let locations = if match_lines.is_empty() {
            String::new()
        } else {
            format!(
                "\nmatches near lines: {}",
                match_lines
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let hint = if found > want {
            "\nhint: add more context in oldText to uniquify, set replace_all, or use start_line/end_line to restrict"
        } else {
            ""
        };
        return ToolResult::error(format!(
            "expected {want} match(es) but found {found} in {}{range_label}{locations}{hint}",
            path.display(),
        ));
    }

    if opts.start_line.is_some() && content.contains(old_text) {
        return ToolResult::error(format!(
            "oldText found in {} but outside{range_label}\n\
             hint: widen start_line/end_line, or drop them",
            path.display()
        ));
    }

    let hint = find_near_miss(&search_content, old_text);
    let line_offset = opts.start_line.unwrap_or(1) - 1;
    let display = path.display();

    match hint {
        NearMiss::WhitespaceDifference(line_num) => ToolResult::error(format!(
            "oldText not found in {display}{range_label}\n\
             near-miss: whitespace-normalised match near line {}\n\
             hint: check for extra/missing spaces, tabs, or trailing whitespace",
            line_num + line_offset
        )),
        NearMiss::SimilarLines(lines) => {
            let mut msg =
                format!("oldText not found in {display}{range_label}\n\nsimilar lines:\n");
            for (num, line) in lines {
                msg.push_str(&format!("  {}: {line}\n", num + line_offset));
            }
            msg.push_str("\nhint: use Read to check exact file contents, then retry");
            ToolResult::error(msg)
        }
        NearMiss::None => ToolResult::error(format!(
            "oldText not found in {display}{range_label}\n\
             hint: use Read to check exact file contents, then retry"
        )),
    }
}

enum NearMiss {
    WhitespaceDifference(usize),
    SimilarLines(Vec<(usize, String)>),
    None,
}

fn fold_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00a0}' | '\u{2007}' | '\u{202f}' | '\u{2000}'..='\u{200a}' => ' ',
        other => other,
    }
}

#[derive(Clone, Copy)]
struct FoldedChar {
    ch: char,
    from: usize,
    to: usize,
    opens: bool,
}

fn fold(s: &str) -> Vec<FoldedChar> {
    let mut folded = Vec::new();
    let mut from = 0;
    for grapheme in s.graphemes(true) {
        let to = from + grapheme.len();
        let mut opens = true;
        for ch in grapheme.nfkc().map(fold_char) {
            folded.push(FoldedChar {
                ch,
                from,
                to,
                opens,
            });
            opens = false;
        }
        from = to;
    }
    folded
}

fn find_folded_matches(content: &str, old_text: &str) -> Vec<String> {
    if content.is_ascii() && old_text.is_ascii() {
        return Vec::new();
    }

    let needle = fold(old_text);
    let hay = fold(content);
    if needle.is_empty() || hay.len() < needle.len() {
        return Vec::new();
    }

    let mut results = Vec::new();
    for start in 0..=hay.len() - needle.len() {
        let end = start + needle.len();
        if !hay[start].opens || hay.get(end).is_some_and(|next| !next.opens) {
            continue;
        }
        if (0..needle.len()).all(|k| hay[start + k].ch == needle[k].ch) {
            let source = &content[hay[start].from..hay[end - 1].to];
            if source != old_text {
                results.push(source.to_string());
            }
        }
    }
    results.dedup();
    results
}

fn find_line_trimmed_matches(content: &str, old_text: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.split_inclusive('\n').collect();
    let search_lines: Vec<&str> = old_text.split_inclusive('\n').collect();

    if search_lines.is_empty() || content_lines.len() < search_lines.len() {
        return vec![];
    }

    let mut results = Vec::new();
    let max_start = content_lines.len() - search_lines.len();

    'outer: for i in 0..=max_start {
        for (j, search_line) in search_lines.iter().enumerate() {
            let content_line = content_lines[i + j];
            if content_line.trim_end() != search_line.trim_end() {
                continue 'outer;
            }
        }

        let start: usize = content_lines[..i].iter().map(|l| l.len()).sum();
        let mut end: usize = content_lines[..i + search_lines.len()]
            .iter()
            .map(|l| l.len())
            .sum();

        if !old_text.ends_with('\n') {
            let last_content_line = content_lines[i + search_lines.len() - 1];
            let trimmed_len = last_content_line.trim_end().len();
            end = content_lines[..i + search_lines.len() - 1]
                .iter()
                .map(|l| l.len())
                .sum::<usize>()
                + trimmed_len;
        }

        results.push(content[start..end].to_string());
    }

    results
}

fn find_near_miss(content: &str, old_text: &str) -> NearMiss {
    let normalise = |s: &str| -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") };
    let norm_old = normalise(old_text);
    if !norm_old.is_empty() {
        let norm_content = normalise(content);
        if norm_content.contains(&norm_old) {
            let line_num = content
                .lines()
                .enumerate()
                .find(|(_, line)| {
                    normalise(line)
                        .contains(normalise(old_text.lines().next().unwrap_or("")).as_str())
                })
                .map(|(i, _)| i + 1)
                .unwrap_or(1);
            return NearMiss::WhitespaceDifference(line_num);
        }
    }

    let first_old_line = old_text.lines().next().unwrap_or("").trim();
    if first_old_line.len() < 4 {
        return NearMiss::None;
    }

    let mut scored: Vec<(usize, &str, f64)> = content
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let score = line_similarity(first_old_line, line.trim());
            if score > 0.4 {
                Some((i + 1, line, score))
            } else {
                Option::None
            }
        })
        .collect();

    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(3);

    if scored.is_empty() {
        NearMiss::None
    } else {
        NearMiss::SimilarLines(
            scored
                .into_iter()
                .map(|(num, line, _)| (num, line.to_string()))
                .collect(),
        )
    }
}

fn line_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let bigrams = |s: &str| -> std::collections::HashSet<(char, char)> {
        s.chars().zip(s.chars().skip(1)).collect()
    };

    let a_bi = bigrams(a);
    let b_bi = bigrams(b);

    if a_bi.is_empty() || b_bi.is_empty() {
        return 0.0;
    }

    let intersection = a_bi.intersection(&b_bi).count() as f64;
    let union = a_bi.union(&b_bi).count() as f64;

    intersection / union
}

fn dedent_pair(old_text: &str, new_text: &str) -> (String, String) {
    let min_indent = old_text
        .lines()
        .chain(new_text.lines())
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    if min_indent == 0 {
        return (old_text.to_string(), new_text.to_string());
    }

    let strip = |text: &str| -> String {
        text.lines()
            .map(|l| {
                if l.len() >= min_indent {
                    &l[min_indent..]
                } else {
                    l.trim_start()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    (strip(old_text), strip(new_text))
}

fn format_edit_diff(old_text: &str, new_text: &str) -> String {
    let (old_text, new_text) = dedent_pair(old_text, new_text);

    if let Some(added) = new_text.strip_prefix(old_text.as_str())
        && !added.trim().is_empty()
    {
        return added
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| format!("+ {l}"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(added) = new_text.strip_suffix(old_text.as_str())
        && !added.trim().is_empty()
    {
        return added
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| format!("+ {l}"))
            .collect::<Vec<_>>()
            .join("\n");
    }

    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(&old_text, &new_text);
    let mut result = String::new();

    let changes: Vec<_> = diff.iter_all_changes().collect();

    const CONTEXT: usize = 2;
    let mut dominated = vec![false; changes.len()];
    for (i, change) in changes.iter().enumerate() {
        if change.tag() != ChangeTag::Equal {
            let start = i.saturating_sub(CONTEXT);
            let end = (i + CONTEXT + 1).min(changes.len());
            for slot in &mut dominated[start..end] {
                *slot = true;
            }
        }
    }

    let mut last_was_gap = false;
    for (i, change) in changes.iter().enumerate() {
        match change.tag() {
            ChangeTag::Equal => {
                if dominated[i] {
                    result.push_str(&format!("  {}", change));
                    last_was_gap = false;
                } else if !last_was_gap && i > 0 && i < changes.len() - 1 {
                    result.push_str("  …\n");
                    last_was_gap = true;
                }
            }
            ChangeTag::Delete => {
                result.push_str(&format!("- {}", change));
                last_was_gap = false;
            }
            ChangeTag::Insert => {
                result.push_str(&format!("+ {}", change));
                last_was_gap = false;
            }
        }
    }

    result.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    #[test]
    fn edit_replaces_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {\n    println!(\"hello\");\n}").unwrap();

        let result = edit_file(
            &path,
            "println!(\"hello\")",
            "println!(\"world\")",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_success());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));

        let output = result
            .content
            .iter()
            .find_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(output.contains("- "));
        assert!(output.contains("+ "));
    }

    #[test]
    fn edit_fails_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = edit_file(
            &path,
            "nonexistent text",
            "replacement",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_fails_on_multiple_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "foo bar foo bar").unwrap();

        let result = edit_file(&path, "foo", "baz", &EditOpts::default());
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_nonexistent_file() {
        let result = edit_file(
            Path::new("/nonexistent/file.rs"),
            "old",
            "new",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_preserves_surrounding_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "before\ntarget line\nafter").unwrap();

        let result = edit_file(&path, "target line", "replaced line", &EditOpts::default());
        assert!(result.outcome.is_success());

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "before\nreplaced line\nafter");
    }

    #[test]
    fn edit_supports_lf_old_text_on_crlf_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "before\r\ntarget line\r\nafter\r\n").unwrap();

        let result = edit_file(
            &path,
            "target line\nafter",
            "replaced\nafter",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_success());

        let bytes = fs::read(&path).unwrap();
        let content = String::from_utf8(bytes).unwrap();
        assert!(content.contains("before\r\nreplaced\r\nafter\r\n"));
    }

    #[test]
    fn edit_preserves_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"hello\nworld\n");
        fs::write(&path, bytes).unwrap();

        let result = edit_file(&path, "hello", "hi", &EditOpts::default());
        assert!(result.outcome.is_success());

        let out = fs::read(&path).unwrap();
        assert!(out.starts_with(&[0xEF, 0xBB, 0xBF]));
        let text = String::from_utf8(out[3..].to_vec()).unwrap();
        assert_eq!(text, "hi\nworld\n");
    }

    #[test]
    fn edit_fails_on_identical_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.txt");
        fs::write(&path, "unchanged").unwrap();

        let result = edit_file(&path, "unchanged", "unchanged", &EditOpts::default());
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_on_empty_file_returns_error_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        fs::write(&path, "").unwrap();

        let result = edit_file(&path, "missing", "present", &EditOpts::default());
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_oldtext_longer_than_file_returns_error_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.txt");
        fs::write(&path, "single line\n").unwrap();

        let result = edit_file(
            &path,
            "line 1\nline 2\nline 3\nline 4",
            "replaced",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_error());
    }

    #[test]
    fn find_line_trimmed_matches_empty_content_does_not_panic() {
        let matches = find_line_trimmed_matches("", "anything");
        assert!(matches.is_empty());
    }

    #[test]
    fn find_line_trimmed_matches_content_shorter_than_oldtext_does_not_panic() {
        let matches = find_line_trimmed_matches("one\n", "one\ntwo\nthree\n");
        assert!(matches.is_empty());
    }

    #[test]
    fn diff_shows_additions_only_when_appending() {
        let diff = format_edit_diff("existing line", "existing line\nnew line");
        assert!(diff.contains("+ new line"));
        assert!(!diff.contains("- "));
    }

    #[test]
    fn diff_shows_additions_only_when_prepending() {
        let diff = format_edit_diff("existing line", "new line\nexisting line");
        assert!(diff.contains("+ new line"));
        assert!(!diff.contains("- "));
    }

    #[test]
    fn diff_shows_both_for_replacement() {
        let diff = format_edit_diff("old line", "new line");
        assert!(diff.contains("- old line"));
        assert!(diff.contains("+ new line"));
    }

    #[test]
    fn diff_dedents_common_whitespace() {
        let diff = format_edit_diff(
            "        deeply indented\n        old code",
            "        deeply indented\n        new code",
        );
        assert!(diff.contains("- old code"));
        assert!(diff.contains("+ new code"));
        assert!(!diff.contains("        "));
    }

    #[test]
    fn diff_dedent_preserves_relative_indent() {
        let diff = format_edit_diff(
            "    base\n        nested",
            "    base\n            more nested",
        );
        assert!(diff.contains("  base"), "context line: {diff}");
        assert!(
            !diff.contains("- base"),
            "unchanged line should not be removed: {diff}"
        );
        assert!(diff.contains("-     nested"), "removed: {diff}");
        assert!(diff.contains("+         more nested"), "added: {diff}");
    }

    #[test]
    fn diff_no_dedent_when_no_common_indent() {
        let diff = format_edit_diff("old line here", "new line here");
        assert!(diff.contains("- old line here"));
        assert!(diff.contains("+ new line here"));
    }

    #[test]
    fn diff_multiline_replacement() {
        let diff = format_edit_diff("line 1\nline 2", "line A\nline B\nline C");
        assert!(diff.contains("- line 1"));
        assert!(diff.contains("- line 2"));
        assert!(diff.contains("+ line A"));
        assert!(diff.contains("+ line B"));
        assert!(diff.contains("+ line C"));
    }

    #[test]
    fn diff_omits_unchanged_context_lines() {
        let old = "## project tracking\n\n- todo.md is the canonical planning file\n\n## reference repos\n\n- pi-mono\n- opencode\n- codex";
        let new = "## project tracking\n\n- todo.md is the canonical planning file\n\n## gitignored local files\n\n- AGENTS.md\n- untracked-docs/\n- reference repos";
        let diff = format_edit_diff(old, new);
        assert!(
            !diff.contains("- ## project tracking"),
            "unchanged line should not appear as removed: {diff}"
        );
        assert!(
            !diff.contains("+ ## project tracking"),
            "unchanged line should not appear as added: {diff}"
        );
        assert!(
            !diff.contains("- - todo.md"),
            "unchanged line should not appear as removed: {diff}"
        );
        assert!(
            diff.contains("- ## reference repos"),
            "removed line should appear: {diff}"
        );
        assert!(
            diff.contains("+ ## gitignored local files"),
            "added line should appear: {diff}"
        );
    }

    #[test]
    fn an_edit_matches_through_smart_punctuation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quotes.rs");
        fs::write(&path, "let s = \"hi\";\nlet n = 1 - 2;\n").unwrap();

        let result = edit_file(
            &path,
            "let s = \u{201c}hi\u{201d};\nlet n = 1 \u{2013} 2;",
            "let s = \"bye\";",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "let s = \"bye\";\n");
    }

    #[test]
    fn an_edit_matches_through_a_fullwidth_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("width.rs");
        fs::write(&path, "print(x);\n").unwrap();

        let result = edit_file(
            &path,
            "print\u{ff08}x\u{ff09};",
            "print(y);",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "print(y);\n");
    }

    #[test]
    fn an_edit_matches_a_differently_composed_accent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accent.md");
        fs::write(&path, "cafe\u{301} au lait\n").unwrap();

        let result = edit_file(&path, "caf\u{e9} au lait", "tea", &EditOpts::default());
        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "tea\n");
    }

    #[test]
    fn a_fold_match_cannot_start_mid_grapheme() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ligature.md");
        fs::write(&path, "\u{fb01}ne\n").unwrap();

        let result = edit_file(&path, "i", "x", &EditOpts::default());
        assert!(result.outcome.is_error(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "\u{fb01}ne\n");
    }

    #[test]
    fn a_folded_match_leaves_the_rest_of_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.md");
        let untouched = "keep \u{fb01}ne \u{201c}quotes\u{201d} and cafe\u{301}\n";
        fs::write(&path, format!("target\n{untouched}")).unwrap();

        let result = edit_file(&path, "target", "hit", &EditOpts::default());
        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("hit\n{untouched}")
        );
    }

    #[test]
    fn an_edit_matches_through_a_non_breaking_space() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nbsp.md");
        fs::write(&path, "a b c\n").unwrap();

        let result = edit_file(&path, "a\u{a0}b c", "done", &EditOpts::default());
        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "done\n");
    }

    #[test]
    fn a_match_outside_the_range_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ranged.rs");
        fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

        let result = edit_file(
            &path,
            "four\nfive",
            "gone",
            &EditOpts {
                start_line: Some(1),
                end_line: Some(3),
                ..EditOpts::default()
            },
        );

        assert!(!result.outcome.is_success());
        let text = result
            .content
            .iter()
            .find_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(
            text.contains("outside") && text.contains("lines 1-3"),
            "should point at the range, got: {text}"
        );
    }

    #[test]
    fn a_near_miss_in_a_range_reports_a_file_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("offset.rs");
        let mut src = "filler\n".repeat(20);
        src.push_str("let  x  =  1;\n");
        fs::write(&path, &src).unwrap();

        let result = edit_file(
            &path,
            "let x = 1;",
            "let x = 2;",
            &EditOpts {
                start_line: Some(20),
                end_line: Some(21),
                ..EditOpts::default()
            },
        );

        assert!(!result.outcome.is_success());
        let text = result
            .content
            .iter()
            .find_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(text.contains("near-miss"), "expected a near-miss: {text}");
        assert!(
            text.contains("line 21"),
            "near-miss line should be a file line, got: {text}"
        );
    }

    #[test]
    fn an_edit_accepts_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliased.rs");
        fs::write(&path, "before\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "file_path": "aliased.rs",
                "oldText": "before",
                "newText": "after",
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
    }

    #[test]
    fn one_call_applies_several_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.rs");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "path": "many.rs",
                "edits": [
                    {"oldText": "one", "newText": "1"},
                    {"oldText": "three", "newText": "3"},
                ],
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "1\ntwo\n3\n");
    }

    #[test]
    fn edits_arrive_as_a_json_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stringified.rs");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "path": "stringified.rs",
                "edits": r#"[{"oldText":"one","newText":"1"},{"oldText":"three","newText":"3"}]"#,
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "1\ntwo\n3\n");
    }

    #[test]
    fn an_edit_accepts_claude_code_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("named.rs");
        fs::write(&path, "before\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "file_path": "named.rs",
                "old_string": "before",
                "new_string": "after",
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
    }

    #[test]
    fn replace_all_takes_every_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rename.rs");
        fs::write(&path, "old\nkeep\nold\nold\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "path": "rename.rs",
                "old_string": "old",
                "new_string": "new",
                "replace_all": true,
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\nkeep\nnew\nnew\n");
    }

    #[test]
    fn an_edit_accepts_expected_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counted.rs");
        fs::write(&path, "x\ny\nx\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "path": "counted.rs",
                "oldText": "x",
                "newText": "z",
                "expected_replacements": 2,
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "z\ny\nz\n");
    }

    #[test]
    fn an_entry_accepts_claude_code_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.rs");
        fs::write(&path, "one\ntwo\nthree\ntwo\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "file_path": "multi.rs",
                "edits": [
                    {"old_string": "one", "new_string": "1"},
                    {"old_string": "two", "new_string": "2", "replace_all": true},
                ],
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "1\n2\nthree\n2\n");
    }

    #[test]
    fn an_entry_without_replace_all_still_wants_one_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambiguous.rs");
        fs::write(&path, "dup\nkeep\ndup\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "path": "ambiguous.rs",
                "edits": [
                    {"oldText": "dup", "newText": "x"},
                    {"oldText": "keep", "newText": "y"},
                ],
            })));

        assert!(!result.outcome.is_success());
        assert_eq!(fs::read_to_string(&path).unwrap(), "dup\nkeep\ndup\n");
    }

    #[test]
    fn a_failing_edit_in_a_batch_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.rs");
        fs::write(&path, "one\ntwo\n").unwrap();
        let tool = EditTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "path": "atomic.rs",
                "edits": [
                    {"oldText": "one", "newText": "1"},
                    {"oldText": "absent", "newText": "x"},
                ],
            })));

        assert!(!result.outcome.is_success());
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn near_miss_whitespace_difference() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let old_text = "fn main() {\n        println!(\"hello\");\n}\n";
        match find_near_miss(content, old_text) {
            NearMiss::WhitespaceDifference(_) => {}
            NearMiss::SimilarLines(_) => panic!("expected WhitespaceDifference, got SimilarLines"),
            NearMiss::None => panic!("expected WhitespaceDifference, got None"),
        }
    }

    #[test]
    fn near_miss_similar_lines() {
        let content = "fn process_data(input: &str) -> Result<(), Error> {\n    validate(input)?;\n    Ok(())\n}\n";
        let old_text = "fn process_data(input: &str) -> Result<(), MyError> {";
        match find_near_miss(content, old_text) {
            NearMiss::SimilarLines(lines) => {
                assert!(!lines.is_empty());
                assert!(lines[0].1.contains("process_data"));
            }
            NearMiss::WhitespaceDifference(_) => {
                panic!("expected SimilarLines, got WhitespaceDifference")
            }
            NearMiss::None => panic!("expected SimilarLines, got None"),
        }
    }

    #[test]
    fn near_miss_nothing_close() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let old_text = "completely unrelated text that appears nowhere";
        assert!(matches!(find_near_miss(content, old_text), NearMiss::None));
    }

    #[test]
    fn edit_no_match_shows_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(
            &path,
            "fn process(input: &str) {\n    validate(input);\n}\n",
        )
        .unwrap();

        let result = edit_file(
            &path,
            "fn process(input: &String) {",
            "fn replaced() {",
            &EditOpts::default(),
        );
        let text = extract_text(&result);
        assert!(text.contains("similar lines") || text.contains("whitespace"));
    }

    #[test]
    fn edit_multiple_match_shows_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "let x = 1;\nlet y = 2;\nlet x = 1;\n").unwrap();

        let result = edit_file(&path, "let x = 1;", "let x = 42;", &EditOpts::default());
        let text = extract_text(&result);
        assert!(
            text.contains("expected 1") && text.contains("found 2"),
            "got: {text}"
        );
        assert!(text.contains("lines:"));
    }

    #[test]
    fn similarity_identical() {
        assert!((line_similarity("hello world", "hello world") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn similarity_empty() {
        assert_eq!(line_similarity("", "hello"), 0.0);
        assert_eq!(line_similarity("hello", ""), 0.0);
    }

    #[test]
    fn edit_trailing_newline_normalisation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {\n    hello();\n}\n").unwrap();

        let result = edit_file(
            &path,
            "fn main() {\n    hello();\n}",
            "fn main() {\n    world();\n}",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_success());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("world()"));
    }

    #[test]
    fn edit_extra_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "hello world").unwrap();

        let result = edit_file(
            &path,
            "hello world\n",
            "goodbye world\n",
            &EditOpts::default(),
        );
        assert!(result.outcome.is_success());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("goodbye world"));
    }

    #[test]
    fn edit_matches_with_trailing_whitespace_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {  \n    println!(\"hello\");  \n}\n").unwrap();

        let result = edit_file(
            &path,
            "fn main() {\n    println!(\"hello\");\n}\n",
            "fn main() {\n    println!(\"goodbye\");\n}\n",
            &EditOpts::default(),
        );
        let text = extract_text(&result);
        assert!(!text.contains("not found"), "should match: {text}");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("goodbye"));
        assert!(!content.contains("hello"));
    }

    #[test]
    fn edit_matches_trailing_tabs_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "line one\t\nline two\t\t\n").unwrap();

        let result = edit_file(
            &path,
            "line one\nline two\n",
            "replaced one\nreplaced two\n",
            &EditOpts::default(),
        );
        let text = extract_text(&result);
        assert!(!text.contains("not found"), "should match: {text}");

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "replaced one\nreplaced two\n");
    }

    #[test]
    fn edit_trailing_ws_no_match_when_content_differs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "foo  \nbar  \n").unwrap();

        let result = edit_file(&path, "fox\nbaz\n", "replaced\n", &EditOpts::default());
        let text = extract_text(&result);
        assert!(text.contains("not found"));
    }

    #[test]
    fn edit_trailing_ws_rejects_ambiguous_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "foo  \nbar\nfoo  \nbaz\n").unwrap();

        let result = edit_file(&path, "foo\n", "replaced\n", &EditOpts::default());
        let text = extract_text(&result);
        assert!(
            text.contains("not found") || text.contains("expected 1"),
            "got: {text}"
        );
    }

    #[test]
    fn edit_expected_matches_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "TODO: fix\nsome code\nTODO: fix\n").unwrap();

        let result = edit_file(
            &path,
            "TODO: fix",
            "DONE",
            &EditOpts {
                matches: Matches::Exactly(2),
                ..Default::default()
            },
        );
        assert!(result.outcome.is_success());
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "DONE\nsome code\nDONE\n");
    }

    #[test]
    fn edit_expected_matches_mismatch_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "foo\nbar\nfoo\nbaz\nfoo\n").unwrap();

        let result = edit_file(
            &path,
            "foo",
            "replaced",
            &EditOpts {
                matches: Matches::Exactly(2),
                ..Default::default()
            },
        );
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(
            text.contains("expected 2") && text.contains("found 3"),
            "got: {text}"
        );
    }

    fn temp_git_repo_for_edit() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .arg("-q")
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        Some(dir)
    }

    #[test]
    fn edit_success_appends_gitignore_hint_for_ignored_files() {
        let Some(dir) = temp_git_repo_for_edit() else {
            return;
        };
        fs::write(dir.path().join(".gitignore"), "*.local\n").unwrap();
        let path = dir.path().join("notes.local");
        fs::write(&path, "old text\n").unwrap();

        let result = edit_file(&path, "old text", "new text", &EditOpts::default());
        assert!(!result.outcome.is_error(), "edit should succeed");
        let text = extract_text(&result);
        assert!(
            text.contains("gitignored"),
            "expected gitignored hint, got: {text}"
        );
    }

    #[test]
    fn edit_success_omits_gitignore_hint_for_tracked_files() {
        let Some(dir) = temp_git_repo_for_edit() else {
            return;
        };
        let path = dir.path().join("README.md");
        fs::write(&path, "old\n").unwrap();

        let result = edit_file(&path, "old", "new", &EditOpts::default());
        assert!(!result.outcome.is_error());
        let text = extract_text(&result);
        assert!(
            !text.contains("gitignored"),
            "tracked file should not get gitignore hint, got: {text}"
        );
    }

    #[test]
    fn edit_range_limited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "let x = 1;\nlet y = 2;\nlet x = 1;\nlet z = 3;\n").unwrap();

        let result = edit_file(
            &path,
            "let x = 1;",
            "let x = 99;",
            &EditOpts {
                matches: Matches::Exactly(1),
                start_line: Some(2),
                end_line: Some(4),
            },
        );
        assert!(
            result.outcome.is_success(),
            "got: {}",
            extract_text(&result)
        );
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "let x = 1;\nlet y = 2;\nlet x = 99;\nlet z = 3;\n");
    }

    async fn read_then_edit(dir: &Path, path: &Path, touch_between: bool) -> ToolResult {
        let ledger = crate::read_ledger::ReadLedger::default();
        let read = crate::read::ReadTool::new(dir.into()).with_read_ledger(ledger.clone());
        let edit = super::EditTool::new(dir.into()).with_read_ledger(ledger);
        let read_result = read
            .execute(serde_json::json!({ "path": path.to_str().unwrap() }))
            .await;
        assert!(read_result.outcome.is_success());
        if touch_between {
            filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(4_102_444_800, 0))
                .unwrap();
        }
        edit.execute(serde_json::json!({
            "path": path.to_str().unwrap(),
            "oldText": "hello",
            "newText": "hi",
        }))
        .await
    }

    #[tokio::test]
    async fn an_edit_after_a_foreign_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "hello\n").unwrap();

        let result = read_then_edit(dir.path(), &path, true).await;

        assert!(result.outcome.is_error());
        assert!(extract_text(&result).contains("read it again"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn an_edit_on_an_unchanged_file_goes_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "hello\n").unwrap();

        let result = read_then_edit(dir.path(), &path, false).await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi\n");
    }

    #[tokio::test]
    async fn two_edits_in_a_row_do_not_trip_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "one two\n").unwrap();
        let ledger = crate::read_ledger::ReadLedger::default();
        let read = crate::read::ReadTool::new(dir.path().into()).with_read_ledger(ledger.clone());
        let edit = super::EditTool::new(dir.path().into()).with_read_ledger(ledger);
        let read_result = read
            .execute(serde_json::json!({ "path": path.to_str().unwrap() }))
            .await;
        assert!(read_result.outcome.is_success());

        let first = edit
            .execute(serde_json::json!({
                "path": path.to_str().unwrap(),
                "oldText": "one",
                "newText": "1",
            }))
            .await;
        let second = edit
            .execute(serde_json::json!({
                "path": path.to_str().unwrap(),
                "oldText": "two",
                "newText": "2",
            }))
            .await;

        assert!(first.outcome.is_success());
        assert!(second.outcome.is_success(), "{}", extract_text(&second));
        assert_eq!(fs::read_to_string(&path).unwrap(), "1 2\n");
    }

    #[tokio::test]
    async fn edit_invalid_line_range_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "hello\nworld\n").unwrap();

        let tool = super::EditTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "path": path.to_str().unwrap(),
                "oldText": "hello",
                "newText": "hi",
                "end_line": 5,
            }))
            .await;
        assert!(result.outcome.is_error());
        assert!(extract_text(&result).contains("requires start_line"));

        let result = tool
            .execute(serde_json::json!({
                "path": path.to_str().unwrap(),
                "oldText": "hello",
                "newText": "hi",
                "start_line": 5,
                "end_line": 2,
            }))
            .await;
        assert!(result.outcome.is_error());
        assert!(extract_text(&result).contains("must be >="));
    }

    #[tokio::test]
    async fn edit_accepts_stringified_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "let x = 1;\nlet x = 1;\nlet x = 1;\n").unwrap();

        let tool = super::EditTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "path": path.to_str().unwrap(),
                "oldText": "let x = 1;",
                "newText": "let x = 9;",
                "start_line": "2",
                "end_line": "2",
            }))
            .await;
        assert!(
            result.outcome.is_success(),
            "got: {}",
            extract_text(&result)
        );
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "let x = 1;\nlet x = 9;\nlet x = 1;\n");
    }

    #[test]
    fn edit_trailing_ws_without_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "alpha  \nbeta  \ngamma\n").unwrap();

        let result = edit_file(&path, "alpha\nbeta", "one\ntwo", &EditOpts::default());
        let text = extract_text(&result);
        assert!(!text.contains("not found"), "should match: {text}");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("one\ntwo"));
    }

    #[test]
    fn edit_args_into_opts_returns_typed_error() {
        let result = EditArgs {
            path: "todo.md".into(),
            old_text: Some("before".into()),
            new_text: Some("after".into()),
            edits: Vec::new(),
            expected_matches: None,
            replace_all: false,
            start_line: None,
            end_line: Some(5),
        }
        .into_opts();

        match result {
            Err(error) => {
                assert_eq!(error, EditArgsError::EndLineRequiresStartLine);
                assert_eq!(error.to_string(), "end_line requires start_line");
            }
            Ok(_) => panic!("expected edit args error"),
        }
    }
}
