//! theme system - configurable colours for the TUI
//!
//! themes are loaded from ~/.config/mush/theme.toml or can be set
//! programmatically. falls back to sensible defaults.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// the full colour theme for the TUI
#[derive(Debug, Clone)]
pub struct Theme {
    pub user_label: Style,
    pub assistant_label: Style,
    pub system_label: Style,
    pub thinking: Style,
    pub thinking_label: Style,
    pub code_block: Style,
    pub inline_code: Style,
    pub heading: Style,
    pub bold: Style,
    pub italic: Style,
    pub tool_running: Style,
    pub tool_done: Style,
    pub tool_error: Style,
    pub tool_output: Style,
    pub usage: Style,
    pub status_model: Style,
    pub status_dim: Style,
    pub input_border: Style,
    pub input_border_active: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_label: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            assistant_label: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            system_label: Style::default().fg(Color::Yellow),
            thinking: Style::default().fg(Color::DarkGray),
            thinking_label: Style::default().fg(Color::DarkGray),
            code_block: Style::default().fg(Color::White),
            inline_code: Style::default().fg(Color::Yellow),
            heading: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            bold: Style::default().add_modifier(Modifier::BOLD),
            italic: Style::default().add_modifier(Modifier::ITALIC),
            tool_running: Style::default().fg(Color::Cyan),
            tool_done: Style::default().fg(Color::Green),
            tool_error: Style::default().fg(Color::Red),
            tool_output: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
            usage: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
            status_model: Style::default().fg(Color::Cyan),
            status_dim: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
            input_border: Style::default().fg(Color::DarkGray),
            input_border_active: Style::default().fg(Color::Blue),
        }
    }
}

/// serialisable theme config (subset of colours)
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub user: Option<String>,
    pub assistant: Option<String>,
    pub system: Option<String>,
    pub thinking: Option<String>,
    pub code: Option<String>,
    pub heading: Option<String>,
    pub tool_running: Option<String>,
    pub tool_done: Option<String>,
    pub tool_error: Option<String>,
    pub status: Option<String>,
    pub border: Option<String>,
}

impl Theme {
    /// load theme from config, applying overrides on top of defaults
    pub fn from_config(config: &ThemeConfig) -> Self {
        let mut theme = Self::default();

        if let Some(c) = config.user.as_deref().and_then(parse_colour) {
            theme.user_label = Style::default().fg(c).add_modifier(Modifier::BOLD);
        }
        if let Some(c) = config.assistant.as_deref().and_then(parse_colour) {
            theme.assistant_label = Style::default().fg(c).add_modifier(Modifier::BOLD);
        }
        if let Some(c) = config.system.as_deref().and_then(parse_colour) {
            theme.system_label = Style::default().fg(c);
        }
        if let Some(c) = config.thinking.as_deref().and_then(parse_colour) {
            theme.thinking = Style::default().fg(c);
            theme.thinking_label = Style::default().fg(c);
        }
        if let Some(c) = config.code.as_deref().and_then(parse_colour) {
            theme.code_block = Style::default().fg(c);
            theme.inline_code = Style::default().fg(c);
        }
        if let Some(c) = config.heading.as_deref().and_then(parse_colour) {
            theme.heading = Style::default().fg(c).add_modifier(Modifier::BOLD);
        }
        if let Some(c) = config.tool_running.as_deref().and_then(parse_colour) {
            theme.tool_running = Style::default().fg(c);
        }
        if let Some(c) = config.tool_done.as_deref().and_then(parse_colour) {
            theme.tool_done = Style::default().fg(c);
        }
        if let Some(c) = config.tool_error.as_deref().and_then(parse_colour) {
            theme.tool_error = Style::default().fg(c);
        }
        if let Some(c) = config.status.as_deref().and_then(parse_colour) {
            theme.status_model = Style::default().fg(c);
        }
        if let Some(c) = config.border.as_deref().and_then(parse_colour) {
            theme.input_border = Style::default().fg(c);
        }

        theme
    }
}

/// parse a colour string into a ratatui Color
fn parse_colour(s: &str) -> Option<Color> {
    // named colours
    match s.to_lowercase().as_str() {
        "black" => return Some(Color::Black),
        "red" => return Some(Color::Red),
        "green" => return Some(Color::Green),
        "yellow" => return Some(Color::Yellow),
        "blue" => return Some(Color::Blue),
        "magenta" => return Some(Color::Magenta),
        "cyan" => return Some(Color::Cyan),
        "white" => return Some(Color::White),
        "gray" | "grey" => return Some(Color::Gray),
        "darkgray" | "darkgrey" => return Some(Color::DarkGray),
        "lightred" => return Some(Color::LightRed),
        "lightgreen" => return Some(Color::LightGreen),
        "lightyellow" => return Some(Color::LightYellow),
        "lightblue" => return Some(Color::LightBlue),
        "lightmagenta" => return Some(Color::LightMagenta),
        "lightcyan" => return Some(Color::LightCyan),
        _ => {}
    }

    // hex colour (#rrggbb)
    if let Some(hex) = s.strip_prefix('#')
        && hex.len() == 6
    {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(r, g, b));
    }

    // 256-colour index
    if let Ok(n) = s.parse::<u8>() {
        return Some(Color::Indexed(n));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_has_colours() {
        let theme = Theme::default();
        assert_eq!(theme.user_label.fg, Some(Color::Green));
        assert_eq!(theme.assistant_label.fg, Some(Color::Blue));
        assert_eq!(theme.tool_error.fg, Some(Color::Red));
    }

    #[test]
    fn parse_named_colours() {
        assert_eq!(parse_colour("red"), Some(Color::Red));
        assert_eq!(parse_colour("Blue"), Some(Color::Blue));
        assert_eq!(parse_colour("darkgray"), Some(Color::DarkGray));
        assert_eq!(parse_colour("darkgrey"), Some(Color::DarkGray));
    }

    #[test]
    fn parse_hex_colour() {
        assert_eq!(parse_colour("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_colour("#00ff00"), Some(Color::Rgb(0, 255, 0)));
        assert_eq!(parse_colour("#1a2b3c"), Some(Color::Rgb(26, 43, 60)));
    }

    #[test]
    fn parse_indexed_colour() {
        assert_eq!(parse_colour("196"), Some(Color::Indexed(196)));
        assert_eq!(parse_colour("0"), Some(Color::Indexed(0)));
    }

    #[test]
    fn parse_invalid_colour() {
        assert_eq!(parse_colour("notacolour"), None);
        assert_eq!(parse_colour("#xyz"), None);
        assert_eq!(parse_colour(""), None);
    }

    #[test]
    fn theme_from_config_overrides() {
        let config = ThemeConfig {
            user: Some("red".into()),
            assistant: Some("#00ff00".into()),
            ..Default::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.user_label.fg, Some(Color::Red));
        assert_eq!(theme.assistant_label.fg, Some(Color::Rgb(0, 255, 0)));
        // unset values keep defaults
        assert_eq!(theme.tool_error.fg, Some(Color::Red));
    }

    #[test]
    fn theme_from_empty_config() {
        let config = ThemeConfig::default();
        let theme = Theme::from_config(&config);
        let default = Theme::default();
        assert_eq!(theme.user_label.fg, default.user_label.fg);
    }
}
