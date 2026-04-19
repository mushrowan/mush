//! theme system - configurable colours for the TUI
//!
//! themes are loaded from config or auto-detected from the terminal
//! background. falls back to sensible ANSI defaults that work on
//! both dark and light terminals.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// how to render +/- lines in diffs.
///
/// `Highlight` tints the whole line with a subtle background colour so the
/// change stands out like github or vscode. `Prefix` keeps only the
/// `+`/`-` prefix + fg colour, matching classic diff output.
///
/// a future `Bar` variant could draw a coloured `▎` bar in the gutter with
/// dim text, but the two styles below cover the main preferences
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineStyle {
    /// classic: `+ text` / `- text` with fg colour only
    Prefix,
    /// github/vscode style: whole-line bg tint for +/- lines
    #[default]
    Highlight,
}

/// the full colour theme for the TUI
#[derive(Debug, Clone)]
pub struct Theme {
    // message labels
    pub user_label: Style,
    pub assistant_label: Style,
    pub system_label: Style,

    // thinking
    pub thinking: Style,
    pub thinking_label: Style,

    // markdown
    pub code_block: Style,
    pub inline_code: Style,
    pub heading: Style,
    pub heading_h3: Style,
    pub bold: Style,
    pub italic: Style,
    pub list_bullet: Style,
    pub horizontal_rule: Style,
    pub link: Style,

    // tool calls
    pub tool_running: Style,
    pub tool_done: Style,
    pub tool_error: Style,
    pub tool_output: Style,

    // usage and stats
    pub usage: Style,
    pub status_model: Style,
    pub status_dim: Style,

    // input box
    pub input_border: Style,
    pub input_border_active: Style,

    // dim text (reusable across widgets)
    pub dim: Style,

    // user message background
    pub user_msg_bg: Style,

    // code block selection highlight
    pub block_highlight_bg: Color,

    // pane separator (unfocused)
    pub pane_separator: Style,
    // pane separator (adjacent to focused)
    pub pane_separator_active: Style,

    // selection indicator in scroll mode
    pub selection_marker: Style,

    // scroll mode hint text
    pub scroll_hint: Style,
    pub scroll_indicator: Style,

    // diff colouring in tool output
    pub diff_added: Style,
    pub diff_removed: Style,
    /// intra-line highlight for word-level changes (differing tokens)
    /// within a paired removed/added line
    pub diff_added_intra: Style,
    pub diff_removed_intra: Style,
    /// subtle line-level bg for Highlight mode. applied to the whole row
    /// (prefix + equal tokens + padding) while intra tokens use the
    /// stronger `*_intra` bg on top
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    /// how to render +/- lines (Prefix: fg only; Highlight: line bg tint)
    pub diff_line_style: DiffLineStyle,
    /// style for the truncation footer ("… (N more lines)") inside a
    /// diff block. full-row style so the footer reads as part of the
    /// block instead of floating below the last coloured row
    pub diff_footer: Style,

    // context pressure colours
    pub context_ok: Style,
    pub context_warn: Style,
    pub context_danger: Style,
    pub context_cold: Style,

    // session picker
    pub picker_selected: Style,
    pub picker_title: Style,

    // search popup
    pub search_border: Style,
    pub search_match: Style,

    // tab bar
    pub tab_active: Style,
    pub tab_inactive: Style,
    pub tab_busy: Style,

    // slash menu
    pub menu_selected_bg: Color,
    pub menu_description: Style,

    // background alert
    pub alert: Style,

    // confirm prompt
    pub confirm: Style,

    // image label
    pub image_label: Style,

    // unread indicator flash
    pub unread: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// effective base style for a diff +/- line. in Highlight mode the
    /// line-level bg is folded into the base fg; in Prefix mode the base
    /// style is fg-only. used by `widgets::diff` for prefix, equal tokens
    /// and padding so the whole row reads as a single tinted band
    #[must_use]
    pub fn diff_base_added(&self) -> Style {
        match self.diff_line_style {
            DiffLineStyle::Prefix => self.diff_added,
            DiffLineStyle::Highlight => self.diff_added.bg(self.diff_added_bg),
        }
    }

    /// see [`Self::diff_base_added`]
    #[must_use]
    pub fn diff_base_removed(&self) -> Style {
        match self.diff_line_style {
            DiffLineStyle::Prefix => self.diff_removed,
            DiffLineStyle::Highlight => self.diff_removed.bg(self.diff_removed_bg),
        }
    }

    /// dark terminal defaults (ANSI colours that adapt to the palette)
    pub fn dark() -> Self {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);

        Self {
            user_label: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            assistant_label: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            system_label: Style::default().fg(Color::Yellow),

            thinking: Style::default()
                .fg(Color::Indexed(103))
                .add_modifier(Modifier::ITALIC),
            thinking_label: Style::default().fg(Color::Indexed(103)),

            code_block: Style::default().fg(Color::White),
            inline_code: Style::default().fg(Color::Yellow),
            heading: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            heading_h3: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            bold: Style::default().add_modifier(Modifier::BOLD),
            italic: Style::default().add_modifier(Modifier::ITALIC),
            list_bullet: Style::default().fg(Color::Cyan),
            horizontal_rule: Style::default().fg(Color::DarkGray),
            link: Style::default().fg(Color::Cyan),

            tool_running: Style::default().fg(Color::Cyan),
            tool_done: Style::default().fg(Color::Green),
            tool_error: Style::default().fg(Color::Red),
            tool_output: dim,

            usage: dim,
            status_model: Style::default().fg(Color::Cyan),
            status_dim: dim,

            input_border: Style::default().fg(Color::DarkGray),
            input_border_active: Style::default().fg(Color::Blue),

            dim,

            // use indexed colour 236 (dark grey) for user bg on dark terms
            user_msg_bg: Style::default().bg(Color::Indexed(236)),
            block_highlight_bg: Color::Indexed(237),

            pane_separator: Style::default().fg(Color::Indexed(236)),
            pane_separator_active: Style::default().fg(Color::DarkGray),

            selection_marker: Style::default().fg(Color::Cyan),
            scroll_hint: Style::default().fg(Color::DarkGray),
            scroll_indicator: Style::default().fg(Color::Blue),

            diff_added: Style::default().fg(Color::Green),
            diff_removed: Style::default().fg(Color::Red),
            // intra-highlight: lighter, less saturated green/red that pop
            // against the very subtle whole-line bg tint below
            diff_added_intra: Style::default()
                .fg(Color::Rgb(150, 210, 150))
                .bg(Color::Rgb(25, 65, 35)),
            diff_removed_intra: Style::default()
                .fg(Color::Rgb(230, 150, 150))
                .bg(Color::Rgb(75, 30, 40)),
            // whole-line bg in Highlight mode: very dark muted green / red,
            // desaturated enough to sit gently on top of the background rather
            // than the saturated primary-colour splashes we had before
            diff_added_bg: Color::Rgb(12, 38, 20),
            diff_removed_bg: Color::Rgb(50, 18, 24),
            diff_line_style: DiffLineStyle::Highlight,
            // neutral dim-grey bg so the footer ties in with the diff
            // block regardless of which +/- tint preceded it
            diff_footer: Style::default().fg(Color::DarkGray).bg(Color::Indexed(236)),

            context_ok: Style::default().fg(Color::Green),
            context_warn: Style::default().fg(Color::Yellow),
            context_danger: Style::default().fg(Color::Red),
            context_cold: Style::default().fg(Color::DarkGray),

            picker_selected: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            picker_title: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),

            search_border: Style::default().fg(Color::Yellow),
            search_match: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),

            tab_active: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            tab_inactive: Style::default().fg(Color::DarkGray),
            tab_busy: Style::default().fg(Color::Yellow),

            menu_selected_bg: Color::DarkGray,
            menu_description: dim,

            alert: Style::default().fg(Color::Yellow),
            confirm: Style::default().fg(Color::Yellow),
            image_label: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::ITALIC),
            unread: Style::default().fg(Color::Yellow),
        }
    }

    /// light terminal defaults
    pub fn light() -> Self {
        let dim = Style::default().fg(Color::Gray).add_modifier(Modifier::DIM);

        Self {
            user_label: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            assistant_label: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            system_label: Style::default().fg(Color::Yellow),

            thinking: Style::default()
                .fg(Color::Indexed(103))
                .add_modifier(Modifier::ITALIC),
            thinking_label: Style::default().fg(Color::Indexed(103)),

            code_block: Style::default().fg(Color::Black),
            inline_code: Style::default().fg(Color::Magenta),
            heading: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            heading_h3: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            bold: Style::default().add_modifier(Modifier::BOLD),
            italic: Style::default().add_modifier(Modifier::ITALIC),
            list_bullet: Style::default().fg(Color::Blue),
            horizontal_rule: Style::default().fg(Color::Gray),
            link: Style::default().fg(Color::Blue),

            tool_running: Style::default().fg(Color::Blue),
            tool_done: Style::default().fg(Color::Green),
            tool_error: Style::default().fg(Color::Red),
            tool_output: dim,

            usage: dim,
            status_model: Style::default().fg(Color::Blue),
            status_dim: dim,

            input_border: Style::default().fg(Color::Gray),
            input_border_active: Style::default().fg(Color::Blue),

            dim,

            // use indexed colour 254 (light grey) for user bg on light terms
            user_msg_bg: Style::default().bg(Color::Indexed(254)),
            block_highlight_bg: Color::Indexed(253),

            pane_separator: Style::default().fg(Color::Indexed(253)),
            pane_separator_active: Style::default().fg(Color::Gray),

            selection_marker: Style::default().fg(Color::Blue),
            scroll_hint: Style::default().fg(Color::Gray),
            scroll_indicator: Style::default().fg(Color::Blue),

            diff_added: Style::default().fg(Color::Green),
            diff_removed: Style::default().fg(Color::Red),
            // light terminals: softer intra tints with darker fg so the
            // changed tokens still pop against the whole-line bg
            diff_added_intra: Style::default()
                .fg(Color::Rgb(25, 90, 40))
                .bg(Color::Rgb(200, 240, 210)),
            diff_removed_intra: Style::default()
                .fg(Color::Rgb(130, 30, 40))
                .bg(Color::Rgb(245, 210, 215)),
            diff_added_bg: Color::Rgb(225, 245, 230),
            diff_removed_bg: Color::Rgb(250, 230, 232),
            diff_line_style: DiffLineStyle::Highlight,
            diff_footer: Style::default().fg(Color::DarkGray).bg(Color::Indexed(253)),

            context_ok: Style::default().fg(Color::Green),
            context_warn: Style::default().fg(Color::Yellow),
            context_danger: Style::default().fg(Color::Red),
            context_cold: Style::default().fg(Color::Gray),

            picker_selected: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            picker_title: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),

            search_border: Style::default().fg(Color::Blue),
            search_match: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),

            tab_active: Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            tab_inactive: Style::default().fg(Color::Gray),
            tab_busy: Style::default().fg(Color::Yellow),

            menu_selected_bg: Color::Indexed(254),
            menu_description: dim,

            alert: Style::default().fg(Color::Yellow),
            confirm: Style::default().fg(Color::Yellow),
            image_label: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::ITALIC),
            unread: Style::default().fg(Color::Yellow),
        }
    }

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
            theme.heading_h3 = Style::default().fg(c).add_modifier(Modifier::BOLD);
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

/// detect whether the terminal has a light background
pub fn detect_background() -> bool {
    // try COLORFGBG first (fast, no escape sequences)
    if let Ok(val) = std::env::var("COLORFGBG")
        && let Some(bg) = val.rsplit(';').next()
        && let Ok(n) = bg.parse::<u8>()
    {
        // ANSI colours 0-6 and 8 are dark, 7 and 9-15 are light
        return n == 7 || n >= 9;
    }

    false
}

/// create a theme based on terminal background detection
pub fn auto_theme(config: &ThemeConfig) -> Theme {
    let base = if detect_background() {
        Theme::light()
    } else {
        Theme::dark()
    };

    // apply config overrides on top of the auto-detected base
    let mut theme = base;

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
        theme.heading_h3 = Style::default().fg(c).add_modifier(Modifier::BOLD);
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
    fn dark_theme_uses_ansi_colours() {
        let theme = Theme::dark();
        // no RGB values in the dark theme defaults
        assert_ne!(theme.user_msg_bg.bg, Some(Color::Rgb(52, 53, 65)));
        // should use indexed colours instead
        assert_eq!(theme.user_msg_bg.bg, Some(Color::Indexed(236)));
    }

    #[test]
    fn light_theme_differs_from_dark() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_ne!(dark.user_msg_bg.bg, light.user_msg_bg.bg);
        assert_ne!(dark.block_highlight_bg, light.block_highlight_bg);
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

    #[test]
    fn auto_theme_applies_overrides() {
        let config = ThemeConfig {
            user: Some("magenta".into()),
            ..Default::default()
        };
        let theme = auto_theme(&config);
        assert_eq!(theme.user_label.fg, Some(Color::Magenta));
    }
}
