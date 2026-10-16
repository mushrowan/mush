mod completions;
mod control;
mod daemon;
mod headless;
mod json_mode;
mod output;
mod sync;

pub(crate) fn tempdir() -> std::io::Result<tempfile::TempDir> {
    if cfg!(target_os = "macos") {
        tempfile::tempdir_in("/private/tmp")
    } else {
        tempfile::tempdir()
    }
}
