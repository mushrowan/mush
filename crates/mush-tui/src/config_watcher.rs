//! watch config.toml for changes and reload theme
//!
//! uses `notify` to watch the config file. when it changes, reloads
//! the theme config and sends it through a channel.

use std::path::PathBuf;
use std::sync::mpsc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::theme::{Theme, ThemeConfig};

/// start watching a config file for changes.
/// returns a receiver that yields new themes when the file changes.
pub fn watch_config(config_path: PathBuf) -> Option<(RecommendedWatcher, mpsc::Receiver<Theme>)> {
    let (tx, rx) = mpsc::channel();
    let path = config_path.clone();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res
            && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
            && let Some(theme) = reload_theme(&path)
        {
            let _ = tx.send(theme);
        }
    })
    .ok()?;

    // watch the parent directory (some editors do atomic saves via rename)
    let watch_dir = config_path.parent().unwrap_or(&config_path);
    watcher.watch(watch_dir, RecursiveMode::NonRecursive).ok()?;

    Some((watcher, rx))
}

fn reload_theme(config_path: &std::path::Path) -> Option<Theme> {
    let content = std::fs::read_to_string(config_path).ok()?;
    let config: ConfigWithTheme = toml::from_str(&content).ok()?;
    Some(crate::theme::auto_theme(&config.theme))
}

/// minimal config struct just for extracting the theme section
#[derive(serde::Deserialize)]
struct ConfigWithTheme {
    #[serde(default)]
    theme: ThemeConfig,
}
