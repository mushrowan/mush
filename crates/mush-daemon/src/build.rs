#[must_use]
pub fn build_id() -> String {
    let exe = std::env::current_exe().ok();
    let modified = exe
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_secs());
    let exe = exe
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    format!("{exe}@{modified}")
}
