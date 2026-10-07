use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use ignore::{Walk, WalkBuilder};

use crate::util::truncate_lines;

const MAX_IGNORED_SUMMARIES: usize = 20;
const MAX_IGNORED_SUMMARY_MATCHES: usize = 500;
const NOUN: &str = "files";

pub(crate) struct PathMatch {
    pub(crate) path: PathBuf,
    pub(crate) display: String,
}

impl PathMatch {
    pub(crate) fn new(cwd: &Path, search_dir: &Path, path: PathBuf) -> Self {
        let display = path
            .strip_prefix(search_dir)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .or_else(|| path.strip_prefix(cwd).ok())
            .unwrap_or(&path)
            .display()
            .to_string();
        Self { path, display }
    }
}

pub(crate) fn walk(search_dir: &Path, include_ignored: bool) -> Walk {
    WalkBuilder::new(search_dir)
        .hidden(!include_ignored)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_exclude(!include_ignored)
        .git_global(!include_ignored)
        .parents(true)
        .require_git(false)
        .build()
}

pub(crate) fn render(
    search_dir: &Path,
    visible: Vec<PathMatch>,
    all: Vec<PathMatch>,
    include_ignored: bool,
) -> String {
    let visible_paths: HashSet<&Path> = visible.iter().map(|entry| entry.path.as_path()).collect();
    let ignored: Vec<PathMatch> = all
        .into_iter()
        .filter(|entry| !visible_paths.contains(entry.path.as_path()))
        .collect();
    drop(visible_paths);
    let mut lines: Vec<String> = visible.into_iter().map(|entry| entry.display).collect();

    if include_ignored {
        lines.extend(
            ignored
                .into_iter()
                .map(|entry| format!("* {}", entry.display)),
        );
        return format_lines_with_ignored_note(&lines);
    }

    let summaries = summarise_ignored(search_dir, ignored.iter().map(|entry| entry.path.as_path()));
    format_lines_with_ignored_summary(&lines, &summaries)
}

struct IgnoredSummary {
    base: String,
    count: usize,
}

fn summarise_ignored<'a>(
    search_dir: &Path,
    ignored: impl Iterator<Item = &'a Path>,
) -> Vec<IgnoredSummary> {
    let mut counts = BTreeMap::<String, usize>::new();
    for path in ignored.take(MAX_IGNORED_SUMMARY_MATCHES) {
        *counts.entry(ignored_base(search_dir, path)).or_default() += 1;
    }

    counts
        .into_iter()
        .take(MAX_IGNORED_SUMMARIES)
        .map(|(base, count)| IgnoredSummary { base, count })
        .collect()
}

fn ignored_base(search_dir: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(search_dir).unwrap_or(path);
    let mut components = relative.components();
    let first = components
        .next()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if path.is_dir() || components.next().is_some() {
        format!("{first}/")
    } else {
        first
    }
}

fn format_lines_with_ignored_summary(lines: &[String], summaries: &[IgnoredSummary]) -> String {
    let mut output = match (lines.is_empty(), summaries.is_empty()) {
        (true, true) => format!("no {NOUN} found"),
        (true, false) => format!("no visible {NOUN} found"),
        (false, _) => truncate_owned_lines(lines),
    };

    if !summaries.is_empty() {
        output.push_str("\n\nignored matches omitted:");
        for summary in summaries {
            let plural = if summary.count == 1 {
                "match"
            } else {
                "matches"
            };
            output.push_str(&format!("\n* {} {} {plural}", summary.base, summary.count));
        }
        output.push_str(
            "\n\nIgnored matches are prefixed with *. Use include_ignored: true to show ignored matches.",
        );
    }

    output
}

fn format_lines_with_ignored_note(lines: &[String]) -> String {
    let mut output = truncate_owned_lines(lines);
    if lines.iter().any(|line| line.starts_with("* ")) {
        output.push_str("\n\nIgnored matches are prefixed with *.");
    }
    output
}

fn truncate_owned_lines(lines: &[String]) -> String {
    let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
    truncate_lines(&borrowed, NOUN)
}
