pub mod app;
pub mod input;
pub mod markdown;
pub mod runner;
pub mod ui;
pub mod widgets;

pub use app::{App, AppEvent};
pub use runner::{TuiConfig, run_tui};
