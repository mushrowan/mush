//! path display utilities shared across widgets

/// replace home dir prefix with ~
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

/// truncate a path from the beginning, keeping the tail
/// e.g. "~/dev/some/deep/nested/project" with max 20 => "…/deep/nested/project"
#[must_use]
pub fn truncate_path(path: &str, max_len: usize) -> String {
    if path.chars().count() <= max_len {
        return path.to_string();
    }
    // find a `/` near the truncation point to get a clean break
    // paths are typically ASCII so byte arithmetic is safe, but use
    // floor_char_boundary to avoid panics on unusual paths
    let target = path.len().saturating_sub(max_len) + 1; // +1 for the `…`
    let target = path.floor_char_boundary(target);
    match path[target..].find('/') {
        Some(pos) => format!("…{}", &path[target + pos..]),
        None => {
            let tail_start = path.len().saturating_sub(max_len.saturating_sub(1));
            let tail_start = path.ceil_char_boundary(tail_start);
            format!("…{}", &path[tail_start..])
        }
    }
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
    fn truncate_long_keeps_tail() {
        let long = "~/dev/some/deep/nested/project";
        let result = truncate_path(long, 20);
        assert!(result.starts_with('…'));
        assert!(result.ends_with("project"));
        assert!(result.len() <= 20);
    }
}
