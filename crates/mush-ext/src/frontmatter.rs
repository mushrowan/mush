//! shared yaml frontmatter extraction

/// extract the yaml text between `---` fences, returns (frontmatter, body_start_index)
///
/// body_start_index is the byte offset after the closing `---` fence
pub fn extract(content: &str) -> Option<(&str, usize)> {
    let trimmed = content.trim_start();
    let offset = content.len() - trimmed.len();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_fence = &trimmed[3..];
    let end = after_fence.find("---")?;
    let body_start = offset + 3 + end + 3;
    Some((after_fence[..end].trim(), body_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let content = "---\nkey: value\n---\nbody here";
        let (fm, idx) = extract(content).unwrap();
        assert_eq!(fm, "key: value");
        assert_eq!(&content[idx..].trim(), &"body here");
    }

    #[test]
    fn no_frontmatter() {
        assert!(extract("no fences here").is_none());
    }

    #[test]
    fn unclosed() {
        assert!(extract("---\nkey: value\nno closing fence").is_none());
    }
}
