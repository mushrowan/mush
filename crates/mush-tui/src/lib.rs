pub mod app;
pub mod app_event;
pub mod app_state;
pub mod at_template;
pub mod batch_output;
pub mod browser_login;
pub mod clipboard;
pub mod config_watcher;
mod conversation_display;
pub mod display_types;
pub mod event_handler;
pub mod fuzzy;
pub mod inline;
pub mod input;
pub mod input_buffer;
pub mod keybinds;
pub mod live_region;
pub mod login_picker;
pub mod markdown;
pub mod model_picker;
pub mod notify;
pub mod path_utils;
pub mod prompt_cache;
pub mod runner;
pub mod screen;
pub mod scrollback;
pub mod serving_provider;
pub mod session;
pub mod session_picker;
pub mod settings;
pub mod slash;
pub mod slash_menu;
pub mod streaming;
pub mod syntax;
pub mod terminal_policy;
#[cfg(test)]
mod test_support;
pub mod text;
pub mod theme;
pub mod ui;
pub mod widgets;

pub use app::{App, AppEvent, ThinkingDisplay};
pub use app_state::StatusBarConfig;
pub use keybinds::{Action as KeybindAction, KeyMap, KeysConfig};
pub use runner::{
    DiscoveryUpdate, LastModels, LastModelsSaver, PaneSnapshot, ReloadCallback, ReloadedContext,
    SessionSaver, SessionSnapshot, ThinkingPrefs, ThinkingPrefsSaver, TuiConfig, canonical_dir,
    run_tui,
};
pub use terminal_policy::{
    IMAGE_PROBE_ENV, ImageProbeMode, KEYBOARD_ENHANCEMENT_ENV, KeyboardEnhancementMode,
    TerminalPolicy, TerminalPolicyOverrides,
};
pub use theme::{Theme, ThemeConfig};
