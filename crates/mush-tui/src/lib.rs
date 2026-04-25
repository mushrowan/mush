pub mod app;
pub mod app_event;
pub mod app_state;
pub mod at_template;
pub mod batch_output;
pub mod cache;
pub mod clipboard;
pub mod config_watcher;
mod conversation_display;
pub mod delegate;
pub mod display_types;
pub mod event_handler;
pub mod file_tracker;
pub mod fuzzy;
pub mod input;
pub mod input_buffer;
pub mod isolation;
pub mod keybinds;
pub mod markdown;
pub mod messaging;
pub mod notify;
pub mod pane;
pub mod path_utils;
pub mod runner;
pub mod session_picker;
pub mod settings;
pub mod shared_state;
pub mod slash;
pub mod slash_menu;
pub mod streaming;
pub mod syntax;
pub mod terminal_policy;
pub mod text;
pub mod theme;
pub mod ui;
pub mod widgets;

pub use app::{App, AppEvent, DEFAULT_SCROLL_LINES, ThinkingDisplay};
pub use app_state::StatusBarConfig;
pub use file_tracker::IsolationMode;
pub use keybinds::{Action as KeybindAction, KeyMap, KeysConfig};
pub use runner::{
    HintMode, LastModelSaver, PaneSnapshot, PromptEnricher, ReloadCallback, ReloadedContext,
    SessionSaver, SessionSnapshot, ThinkingPrefsSaver, TuiConfig, run_tui,
};
pub use terminal_policy::{
    IMAGE_PROBE_ENV, ImageProbeMode, KEYBOARD_ENHANCEMENT_ENV, KeyboardEnhancementMode,
    MOUSE_TRACKING_ENV, MouseTrackingMode, TerminalPolicy, TerminalPolicyOverrides,
};
pub use theme::{Theme, ThemeConfig};
