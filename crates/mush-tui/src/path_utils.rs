#[must_use]
pub fn shorten_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

#[must_use]
pub fn truncate_path(path: &str, max_len: usize) -> String {
    if crate::text::width(path) <= max_len {
        return path.to_string();
    }
    let room = max_len.saturating_sub(1);
    let tail = tail_within(path, room);
    let start = path.len() - tail.len();
    match path[start..].find('/') {
        Some(pos) if pos > 0 => format!("…{}", &path[start + pos..]),
        _ => format!("…{tail}"),
    }
}

fn tail_within(text: &str, max_width: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (at, grapheme) in crate::text::graphemes(text)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        used += crate::text::grapheme_width(grapheme);
        if used > max_width {
            break;
        }
        start = at;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_non_home_path() {
        assert_eq!(shorten_path("/opt/project"), "/opt/project");
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate_path("~/dev/mush", 30), "~/dev/mush");
    }

    #[test]
    fn a_wide_path_is_cut_by_cells() {
        let path = "~/作業/とても長いディレクトリ/プロジェクト";
        let result = truncate_path(path, 16);
        assert_eq!(result, "…/プロジェクト");
        assert!(crate::text::width(&result) <= 16);
    }

    #[test]
    fn truncate_long_keeps_tail() {
        let long = "~/dev/some/deep/nested/project";
        let result = truncate_path(long, 20);
        assert!(result.starts_with('…'));
        assert!(result.ends_with("project"));
        assert!(result.len() <= 20);
    }
}
