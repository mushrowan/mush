mod follow_up;
mod lifecycle;
mod orchestrator_events;
mod review_routing;
mod tasks;

pub(crate) fn tempdir() -> std::io::Result<tempfile::TempDir> {
    if cfg!(target_os = "macos") {
        tempfile::tempdir_in("/private/tmp")
    } else {
        tempfile::tempdir()
    }
}
