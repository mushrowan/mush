use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineStyle {
    Prefix,
    #[default]
    Highlight,
}

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
    pub heading_h3: Style,
    pub bold: Style,
    pub italic: Style,
    pub list_bullet: Style,
    pub horizontal_rule: Style,
    pub link: Style,

    pub tool_running: Style,
    pub tool_done: Style,
    pub tool_error: Style,
    pub tool_output: Style,

    pub usage: Style,
    pub status_model: Style,
    pub status_dim: Style,
    pub status_muted: Style,

    pub input_border: Style,
    pub input_border_active: Style,

    pub dim: Style,

    pub user_msg_bg: Style,

    pub tool_panel_bg: Style,

    pub block_highlight_bg: Color,

    pub pane_separator: Style,
    pub pane_separator_active: Style,

    pub selection_marker: Style,
    pub selection: Style,

    pub scroll_hint: Style,
    pub scroll_indicator: Style,

    pub diff_added: Style,
    pub diff_removed: Style,
    pub diff_added_intra: Style,
    pub diff_removed_intra: Style,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub diff_line_style: DiffLineStyle,
    pub diff_footer: Style,

    pub context_ok: Style,
    pub context_warn: Style,
    pub context_danger: Style,
    pub context_cold: Style,

    pub picker_selected: Style,
    pub picker_title: Style,

    pub search_border: Style,
    pub search_match: Style,

    pub tab_active: Style,
    pub tab_busy: Style,

    pub menu_selected_bg: Color,
    pub menu_description: Style,

    pub alert: Style,

    pub confirm: Style,

    pub image_label: Style,

    pub mode_normal: Style,
    pub mode_visual: Style,

    pub shimmer: Vec<Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    #[must_use]
    pub fn diff_base_added(&self) -> Style {
        match self.diff_line_style {
            DiffLineStyle::Prefix => self.diff_added,
            DiffLineStyle::Highlight => self.diff_added.bg(self.diff_added_bg),
        }
    }

    #[must_use]
    pub fn diff_base_removed(&self) -> Style {
        match self.diff_line_style {
            DiffLineStyle::Prefix => self.diff_removed,
            DiffLineStyle::Highlight => self.diff_removed.bg(self.diff_removed_bg),
        }
    }

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
            status_muted: Style::default().fg(Color::Gray),

            input_border: Style::default().fg(Color::DarkGray),
            input_border_active: Style::default().fg(Color::Blue),

            dim,

            user_msg_bg: Style::default().bg(Color::Indexed(236)),
            tool_panel_bg: Style::default().bg(Color::Indexed(234)),
            block_highlight_bg: Color::Indexed(237),

            pane_separator: Style::default().fg(Color::Indexed(236)),
            pane_separator_active: Style::default().fg(Color::DarkGray),

            selection_marker: Style::default().fg(Color::Cyan),
            selection: Style::default().bg(Color::Indexed(60)),
            scroll_hint: Style::default().fg(Color::DarkGray),
            scroll_indicator: Style::default().fg(Color::Blue),

            diff_added: Style::default().fg(Color::Green),
            diff_removed: Style::default().fg(Color::Red),
            diff_added_intra: Style::default()
                .fg(Color::Rgb(150, 210, 150))
                .bg(Color::Rgb(25, 65, 35)),
            diff_removed_intra: Style::default()
                .fg(Color::Rgb(230, 150, 150))
                .bg(Color::Rgb(75, 30, 40)),
            diff_added_bg: Color::Rgb(12, 38, 20),
            diff_removed_bg: Color::Rgb(50, 18, 24),
            diff_line_style: DiffLineStyle::Highlight,
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
            tab_busy: Style::default().fg(Color::Yellow),

            menu_selected_bg: Color::DarkGray,
            menu_description: dim,

            alert: Style::default().fg(Color::Yellow),
            confirm: Style::default().fg(Color::Yellow),
            image_label: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::ITALIC),
            mode_normal: mode_pill(Color::Cyan),
            mode_visual: mode_pill(Color::Magenta),
            shimmer: crate::widgets::shimmer::interpolate_palette(
                &[
                    Color::Rgb(95, 255, 255),
                    Color::Rgb(0, 175, 255),
                    Color::Rgb(135, 95, 255),
                    Color::Rgb(215, 95, 215),
                    Color::Rgb(255, 95, 175),
                ],
                24,
            ),
        }
    }

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
            status_muted: Style::default().fg(Color::DarkGray),

            input_border: Style::default().fg(Color::Gray),
            input_border_active: Style::default().fg(Color::Blue),

            dim,

            user_msg_bg: Style::default().bg(Color::Indexed(254)),
            tool_panel_bg: Style::default().bg(Color::Indexed(253)),
            block_highlight_bg: Color::Indexed(253),

            pane_separator: Style::default().fg(Color::Indexed(253)),
            pane_separator_active: Style::default().fg(Color::Gray),

            selection_marker: Style::default().fg(Color::Blue),
            selection: Style::default().bg(Color::Indexed(147)),
            scroll_hint: Style::default().fg(Color::Gray),
            scroll_indicator: Style::default().fg(Color::Blue),

            diff_added: Style::default().fg(Color::Green),
            diff_removed: Style::default().fg(Color::Red),
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
            tab_busy: Style::default().fg(Color::Yellow),

            menu_selected_bg: Color::Indexed(254),
            menu_description: dim,

            alert: Style::default().fg(Color::Yellow),
            confirm: Style::default().fg(Color::Yellow),
            image_label: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::ITALIC),
            mode_normal: mode_pill(Color::Cyan),
            mode_visual: mode_pill(Color::Magenta),
            shimmer: crate::widgets::shimmer::interpolate_palette(
                &[
                    Color::Rgb(0, 135, 175),
                    Color::Rgb(0, 95, 175),
                    Color::Rgb(95, 95, 175),
                    Color::Rgb(135, 95, 175),
                    Color::Rgb(215, 0, 135),
                ],
                24,
            ),
        }
    }

    pub fn from_config(config: &ThemeConfig) -> Self {
        Self::default().with_config(config)
    }

    #[must_use]
    fn with_config(mut self, config: &ThemeConfig) -> Self {
        if let Some(preset) = config.preset {
            apply_palette(&mut self, &preset.palette());
        }
        apply_palette(&mut self, &config.palette);

        if let Some(c) = config.user.as_deref().and_then(parse_colour) {
            self.user_label = Style::default().fg(c).add_modifier(Modifier::BOLD);
        }
        if let Some(c) = config.assistant.as_deref().and_then(parse_colour) {
            self.assistant_label = Style::default().fg(c).add_modifier(Modifier::BOLD);
        }
        if let Some(c) = config.system.as_deref().and_then(parse_colour) {
            self.system_label = Style::default().fg(c);
        }
        if let Some(c) = config.thinking.as_deref().and_then(parse_colour) {
            self.thinking = Style::default().fg(c);
            self.thinking_label = Style::default().fg(c);
        }
        if let Some(c) = config.code.as_deref().and_then(parse_colour) {
            self.code_block = Style::default().fg(c);
            self.inline_code = Style::default().fg(c);
        }
        if let Some(c) = config.heading.as_deref().and_then(parse_colour) {
            self.heading = Style::default().fg(c).add_modifier(Modifier::BOLD);
            self.heading_h3 = Style::default().fg(c).add_modifier(Modifier::BOLD);
        }
        if let Some(c) = config.tool_running.as_deref().and_then(parse_colour) {
            self.tool_running = Style::default().fg(c);
        }
        if let Some(c) = config.tool_done.as_deref().and_then(parse_colour) {
            self.tool_done = Style::default().fg(c);
        }
        if let Some(c) = config.tool_error.as_deref().and_then(parse_colour) {
            self.tool_error = Style::default().fg(c);
        }
        if let Some(c) = config.status.as_deref().and_then(parse_colour) {
            self.status_model = Style::default().fg(c);
        }
        if let Some(c) = config.border.as_deref().and_then(parse_colour) {
            self.input_border = Style::default().fg(c);
        }

        self
    }
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(default)]
#[allow(non_snake_case)]
pub struct Base24Palette {
    pub base00: Option<String>,
    pub base01: Option<String>,
    pub base02: Option<String>,
    pub base03: Option<String>,
    pub base04: Option<String>,
    pub base05: Option<String>,
    pub base06: Option<String>,
    pub base07: Option<String>,
    pub base08: Option<String>,
    pub base09: Option<String>,
    pub base0A: Option<String>,
    pub base0B: Option<String>,
    pub base0C: Option<String>,
    pub base0D: Option<String>,
    pub base0E: Option<String>,
    pub base0F: Option<String>,
    pub base10: Option<String>,
    pub base11: Option<String>,
    pub base12: Option<String>,
    pub base13: Option<String>,
    pub base14: Option<String>,
    pub base15: Option<String>,
    pub base16: Option<String>,
    pub base17: Option<String>,
}

fn mode_pill(colour: Color) -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(colour)
        .add_modifier(Modifier::BOLD)
}

fn apply_palette(theme: &mut Theme, p: &Base24Palette) {
    let c = |v: &Option<String>| v.as_deref().and_then(parse_colour);
    let fg = |col: Color| Style::default().fg(col);
    let fg_bold = |col: Color| Style::default().fg(col).add_modifier(Modifier::BOLD);
    let pill_text = c(&p.base00).unwrap_or(Color::Black);
    let pill = |col: Color| mode_pill(col).fg(pill_text);

    if let Some(col) = c(&p.base0D) {
        theme.user_label = fg_bold(col);
        theme.status_model = fg(col);
        theme.input_border_active = fg(col);
        theme.link = fg(col).add_modifier(Modifier::UNDERLINED);
        theme.picker_title = fg_bold(col);
        theme.tab_active = fg_bold(col);
        theme.search_border = fg(col);
        theme.pane_separator_active = fg(col);
    }
    if let Some(col) = c(&p.base0B) {
        theme.assistant_label = fg_bold(col);
        theme.tool_done = fg(col);
        theme.diff_added = fg(col);
        theme.diff_added_intra = fg(col);
        theme.context_ok = fg(col);
    }
    if let Some(col) = c(&p.base08) {
        theme.tool_error = fg(col);
        theme.diff_removed = fg(col);
        theme.diff_removed_intra = fg(col);
        theme.context_danger = fg(col);
    }
    if let Some(col) = c(&p.base0A) {
        theme.tool_running = fg(col);
        theme.list_bullet = fg(col);
        theme.selection_marker = fg(col);
        theme.search_match = fg_bold(col);
        theme.context_warn = fg(col);
        theme.scroll_indicator = fg(col);
        theme.tab_busy = fg(col);
    }
    if let Some(col) = c(&p.base0C) {
        theme.mode_normal = pill(col);
        theme.code_block = fg(col);
        theme.inline_code = fg(col);
    }
    if let Some(col) = c(&p.base0E) {
        theme.mode_visual = pill(col);
        theme.heading = fg_bold(col);
        theme.heading_h3 = fg_bold(col);
    }
    if let Some(col) = c(&p.base03) {
        theme.thinking = fg(col);
        theme.thinking_label = fg(col);
        theme.system_label = fg(col);
        theme.dim = fg(col);
        theme.usage = fg(col);
        theme.status_dim = fg(col);
        theme.scroll_hint = fg(col);
        theme.horizontal_rule = fg(col);
        theme.input_border = fg(col);
        theme.pane_separator = fg(col);
        theme.context_cold = fg(col);
    }
    if let Some(col) = c(&p.base04) {
        theme.status_muted = fg(col);
    }
    if let Some(col) = c(&p.base05) {
        theme.tool_output = fg(col);
        theme.bold = fg(col).add_modifier(Modifier::BOLD);
        theme.italic = fg(col).add_modifier(Modifier::ITALIC);
    }
    if let Some(col) = c(&p.base02) {
        theme.block_highlight_bg = col;
        theme.menu_selected_bg = col;
        theme.selection = Style::default().bg(col);
        theme.picker_selected = Style::default().bg(col);
    }
    if let Some(col) = c(&p.base01) {
        theme.user_msg_bg = Style::default().bg(col);
    }
    if let Some(col) = c(&p.base00) {
        theme.tool_panel_bg = Style::default().bg(col);
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    Amethyst,
}

impl ThemePreset {
    #[must_use]
    pub fn palette(self) -> Base24Palette {
        match self {
            Self::Amethyst => Base24Palette {
                base00: Some("#0d0d10".into()),
                base01: Some("#1a1a1f".into()),
                base02: Some("#222226".into()),
                base03: Some("#71717a".into()),
                base05: Some("#f4f4f5".into()),
                base08: Some("#d06f82".into()),
                base0A: Some("#f59e0b".into()),
                base0B: Some("#7da876".into()),
                base0C: Some("#8ba888".into()),
                base0D: Some("#7c6faf".into()),
                base0E: Some("#8d7fc0".into()),
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Default, Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ThemeConfig {
    pub preset: Option<ThemePreset>,
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
    #[serde(default)]
    pub palette: Base24Palette,
}

fn parse_colour(s: &str) -> Option<Color> {
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

    if let Some(hex) = s.strip_prefix('#')
        && hex.len() == 6
    {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(r, g, b));
    }

    if let Ok(n) = s.parse::<u8>() {
        return Some(Color::Indexed(n));
    }

    None
}

pub fn detect_background() -> bool {
    if let Ok(val) = std::env::var("COLORFGBG")
        && let Some(bg) = val.rsplit(';').next()
        && let Ok(n) = bg.parse::<u8>()
    {
        return n == 7 || n >= 9;
    }

    false
}

pub fn auto_theme(config: &ThemeConfig) -> Theme {
    let base = if detect_background() {
        Theme::light()
    } else {
        Theme::dark()
    };
    base.with_config(config)
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
    fn palette_maps_accents_to_semantic_styles() {
        let config = ThemeConfig {
            palette: Base24Palette {
                base08: Some("#ff0000".into()),
                base0B: Some("#00ff00".into()),
                base0D: Some("#0000ff".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.tool_error.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(theme.tool_done.fg, Some(Color::Rgb(0, 255, 0)));
        assert_eq!(theme.user_label.fg, Some(Color::Rgb(0, 0, 255)));
    }

    #[test]
    fn per_role_override_wins_over_palette() {
        let config = ThemeConfig {
            user: Some("#abcdef".into()),
            palette: Base24Palette {
                base0D: Some("#0000ff".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.user_label.fg, Some(Color::Rgb(0xab, 0xcd, 0xef)));
    }

    #[test]
    fn base16_only_palette_themes_accents() {
        let config = ThemeConfig {
            palette: Base24Palette {
                base0A: Some("#ffff00".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.tool_running.fg, Some(Color::Rgb(255, 255, 0)));
    }

    #[test]
    fn the_amethyst_preset_paints_the_accent_purple() {
        let config = ThemeConfig {
            preset: Some(ThemePreset::Amethyst),
            ..Default::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.user_label.fg, Some(Color::Rgb(0x7c, 0x6f, 0xaf)));
        assert_eq!(theme.tool_panel_bg.bg, Some(Color::Rgb(0x0d, 0x0d, 0x10)));
    }

    #[test]
    fn a_palette_colours_the_mode_pills_on_its_background() {
        let theme = Theme::from_config(&ThemeConfig {
            preset: Some(ThemePreset::Amethyst),
            ..Default::default()
        });
        assert_eq!(theme.mode_normal.bg, Some(Color::Rgb(0x8b, 0xa8, 0x88)));
        assert_eq!(theme.mode_normal.fg, Some(Color::Rgb(0x0d, 0x0d, 0x10)));
        assert_eq!(theme.mode_visual.bg, Some(Color::Rgb(0x8d, 0x7f, 0xc0)));
        assert_eq!(Theme::dark().mode_normal.bg, Some(Color::Cyan));
        assert_eq!(Theme::dark().mode_visual.fg, Some(Color::Black));
    }

    #[test]
    fn a_palette_entry_overrides_the_preset() {
        let config = ThemeConfig {
            preset: Some(ThemePreset::Amethyst),
            palette: Base24Palette {
                base0D: Some("#0000ff".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let theme = Theme::from_config(&config);
        assert_eq!(theme.user_label.fg, Some(Color::Rgb(0, 0, 255)));
    }

    #[test]
    fn the_preset_parses_from_config() {
        let config: ThemeConfig = toml::from_str("preset = \"amethyst\"").unwrap();
        assert_eq!(config.preset, Some(ThemePreset::Amethyst));
    }

    #[test]
    fn dark_theme_uses_ansi_colours() {
        let theme = Theme::dark();
        assert_ne!(theme.user_msg_bg.bg, Some(Color::Rgb(52, 53, 65)));
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

#[cfg(test)]
mod selection_colour_tests {
    use super::*;

    fn rgb(colour: Color) -> (f64, f64, f64) {
        let index = match colour {
            Color::Rgb(r, g, b) => return (f64::from(r), f64::from(g), f64::from(b)),
            Color::Indexed(i) => i,
            other => panic!("expected an indexed or rgb colour, got {other:?}"),
        };
        if index >= 232 {
            let grey = f64::from(8 + (index - 232) * 10);
            return (grey, grey, grey);
        }
        let cube = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
        let i = usize::from(index - 16);
        (cube[i / 36], cube[(i / 6) % 6], cube[i % 6])
    }

    fn luminance(colour: Color) -> f64 {
        let (r, g, b) = rgb(colour);
        0.2126f64.mul_add(r, 0.7152f64.mul_add(g, 0.0722 * b))
    }

    fn assert_selection_stands_out(theme: &Theme, name: &str) {
        let selection = theme.selection.bg.expect("a selection background");
        let backgrounds = [
            ("tool panel", theme.tool_panel_bg.bg),
            ("block highlight", Some(theme.block_highlight_bg)),
            ("user message", theme.user_msg_bg.bg),
        ];
        for (label, background) in backgrounds {
            let background = background.expect("a background");
            let delta = (luminance(selection) - luminance(background)).abs();
            assert!(
                delta >= 28.0,
                "{name}: selection sits {delta:.0} from the {label} background"
            );
        }
    }

    #[test]
    fn the_dark_selection_stands_out() {
        assert_selection_stands_out(&Theme::dark(), "dark");
    }

    #[test]
    fn the_light_selection_stands_out() {
        assert_selection_stands_out(&Theme::light(), "light");
    }
}
