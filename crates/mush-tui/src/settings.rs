use mush_ai::types::AnthropicBetas;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum SettingsScope {
    Global,
    Disabled,
    Repo,
    #[default]
    Session,
}

impl SettingsScope {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Disabled => "disabled",
            Self::Repo => "repo",
            Self::Session => "session",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "global" => Some(Self::Global),
            "disabled" => Some(Self::Disabled),
            "repo" => Some(Self::Repo),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    #[must_use]
    pub fn cycle_next(self) -> Self {
        match self {
            Self::Session => Self::Global,
            Self::Global => Self::Repo,
            Self::Repo => Self::Disabled,
            Self::Disabled => Self::Session,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopedSettings {
    pub scope: SettingsScope,
    pub anthropic_betas: AnthropicBetas,
    pub auto_compact: bool,
    pub price_cliff_pin: mush_session::compact::PriceCliffPin,
    pub preferred_providers: Vec<String>,
}

impl Default for ScopedSettings {
    fn default() -> Self {
        Self {
            scope: SettingsScope::default(),
            anthropic_betas: AnthropicBetas::default(),
            auto_compact: true,
            price_cliff_pin: mush_session::compact::PriceCliffPin::default(),
            preferred_providers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItemKind {
    Scope,
    AutoCompact,
    PriceCliffPin,
    ContextOneM,
    Effort,
    ContextManagement,
    ThinkingTokenCount,
    ThinkingUpdates,
    Advisor,
    AdvancedToolUse,
    FallbackCredit,
}

impl MenuItemKind {
    #[must_use]
    pub fn all() -> &'static [MenuItemKind] {
        &[
            Self::Scope,
            Self::AutoCompact,
            Self::PriceCliffPin,
            Self::ContextOneM,
            Self::Effort,
            Self::ContextManagement,
            Self::ThinkingTokenCount,
            Self::ThinkingUpdates,
            Self::Advisor,
            Self::AdvancedToolUse,
            Self::FallbackCredit,
        ]
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::AutoCompact => "auto_compact",
            Self::PriceCliffPin => "compact_below_price_cliff",
            Self::ContextOneM => "context_1m",
            Self::Effort => "effort",
            Self::ContextManagement => "context_management",
            Self::ThinkingTokenCount => "thinking_token_count",
            Self::ThinkingUpdates => "thinking_updates",
            Self::Advisor => "advisor",
            Self::AdvancedToolUse => "advanced_tool_use",
            Self::FallbackCredit => "fallback_credit",
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Scope => "where /settings changes get persisted",
            Self::AutoCompact => {
                "compact automatically as the context fills, for this session only"
            }
            Self::PriceCliffPin => {
                "compact before a model's price cliff: off, for children, or for every session"
            }
            Self::ContextOneM => "context-1m-2025-08-07: 1M context on claude models",
            Self::Effort => "effort-2025-11-24: send output_config.effort",
            Self::ContextManagement => "context-management-2025-06-27: server-side edits",
            Self::ThinkingTokenCount => "thinking-token-count-2026-05-13",
            Self::ThinkingUpdates => {
                "thinking-display-updates-2026-08-18: show progress updates between tool calls"
            }
            Self::Advisor => "advisor-tool-2026-03-01: advisor tool (not yet supported)",
            Self::AdvancedToolUse => {
                "advanced-tool-use-2025-11-20: advanced tool-use (experimental)"
            }
            Self::FallbackCredit => "fallback-credit-2026-07-01: reprice refusal retries",
        }
    }

    pub fn activate(self, settings: &mut ScopedSettings) -> String {
        match self {
            Self::AutoCompact => settings.auto_compact = !settings.auto_compact,
            Self::PriceCliffPin => settings.price_cliff_pin = settings.price_cliff_pin.cycle_next(),
            _ => match self.beta_mut(&mut settings.anthropic_betas) {
                Some(flag) => *flag = !*flag,
                None => settings.scope = settings.scope.cycle_next(),
            },
        }
        self.value(settings)
    }

    #[must_use]
    pub fn value(self, settings: &ScopedSettings) -> String {
        if self == Self::AutoCompact {
            return bool_str(settings.auto_compact).to_string();
        }
        if self == Self::PriceCliffPin {
            return settings.price_cliff_pin.as_str().to_string();
        }
        let mut betas = settings.anthropic_betas.clone();
        match self.beta_mut(&mut betas) {
            Some(flag) => bool_str(*flag).to_string(),
            None => settings.scope.as_str().to_string(),
        }
    }

    #[must_use]
    pub fn persists(self) -> bool {
        self != Self::AutoCompact
    }

    pub(crate) fn beta_mut(self, betas: &mut AnthropicBetas) -> Option<&mut bool> {
        Some(match self {
            Self::Scope | Self::AutoCompact | Self::PriceCliffPin => return None,
            Self::ContextOneM => &mut betas.context_1m,
            Self::Effort => &mut betas.effort,
            Self::ContextManagement => &mut betas.context_management,
            Self::ThinkingTokenCount => &mut betas.thinking_token_count,
            Self::ThinkingUpdates => &mut betas.thinking_updates,
            Self::Advisor => &mut betas.advisor,
            Self::AdvancedToolUse => &mut betas.advanced_tool_use,
            Self::FallbackCredit => &mut betas.fallback_credit,
        })
    }
}

fn bool_str(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

#[derive(Debug, Clone, Default)]
pub struct SettingsMenuState {
    pub selected: usize,
    pub scroll_offset: usize,
}

impl SettingsMenuState {
    pub fn move_down(&mut self) {
        let max = MenuItemKind::all().len().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn top(&mut self) {
        self.selected = 0;
    }

    pub fn bottom(&mut self) {
        self.selected = MenuItemKind::all().len().saturating_sub(1);
    }

    #[must_use]
    pub fn current(&self) -> MenuItemKind {
        MenuItemKind::all()[self.selected.min(MenuItemKind::all().len() - 1)]
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedSettings {
    pub anthropic_betas: AnthropicBetas,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rlm_max_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_below_price_cliff: Option<mush_session::compact::PriceCliffPin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_providers: Option<Vec<String>>,
}

impl From<&ScopedSettings> for PersistedSettings {
    fn from(s: &ScopedSettings) -> Self {
        Self {
            anthropic_betas: s.anthropic_betas.clone(),
            rlm_max_depth: None,
            compact_below_price_cliff: Some(s.price_cliff_pin),
            preferred_providers: Some(s.preferred_providers.clone()),
        }
    }
}

pub fn persist(
    settings: &ScopedSettings,
    cwd: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, String> {
    match settings.scope {
        SettingsScope::Session => Ok(None),
        SettingsScope::Disabled => Err("settings scope is disabled".into()),
        SettingsScope::Global => {
            let path = global_settings_path().ok_or("could not resolve config dir")?;
            write_settings_file(&path, settings)?;
            Ok(Some(path))
        }
        SettingsScope::Repo => {
            let path = cwd.join(".mush").join("settings.toml");
            write_settings_file(&path, settings)?;
            Ok(Some(path))
        }
    }
}

fn write_settings_file(path: &std::path::Path, settings: &ScopedSettings) -> Result<(), String> {
    let persisted = toml::Table::try_from(PersistedSettings::from(settings))
        .map_err(|e| format!("serialise settings: {e}"))?;
    merge_settings_file(path, persisted)
}

fn merge_settings_file(path: &std::path::Path, entries: toml::Table) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut table = match std::fs::read_to_string(path) {
        Ok(body) => body
            .parse::<toml::Table>()
            .map_err(|e| format!("read {}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    table.extend(entries);
    let body = toml::to_string_pretty(&table).map_err(|e| format!("serialise settings: {e}"))?;
    mush_ai::private_io::write_atomic(path, body)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

pub fn save_global_rlm_max_depth(max_depth: u32) -> Result<std::path::PathBuf, String> {
    let path = global_settings_path().ok_or("could not resolve config dir")?;
    save_rlm_max_depth_to(&path, max_depth)?;
    Ok(path)
}

fn save_rlm_max_depth_to(path: &std::path::Path, max_depth: u32) -> Result<(), String> {
    let mut entries = toml::Table::new();
    entries.insert(
        "rlm_max_depth".into(),
        toml::Value::Integer(i64::from(max_depth)),
    );
    merge_settings_file(path, entries)
}

fn global_settings_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("MUSH_CONFIG_DIR") {
        return Some(std::path::PathBuf::from(dir).join("settings.toml"));
    }
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".config/mush/settings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_parity_toggles_appear_in_the_settings_menu() {
        let settings = ScopedSettings::default();
        let listed: Vec<(&str, String)> = MenuItemKind::all()
            .iter()
            .map(|item| (item.label(), item.value(&settings)))
            .collect();

        for expected in [
            ("thinking_token_count", "on"),
            ("thinking_updates", "on"),
            ("fallback_credit", "off"),
        ] {
            assert!(
                listed
                    .iter()
                    .any(|(l, v)| *l == expected.0 && v == expected.1),
                "{expected:?} missing from {listed:?}"
            );
        }
    }

    #[test]
    fn the_price_cliff_row_cycles_and_is_saved_with_the_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let mut settings = ScopedSettings {
            scope: SettingsScope::Repo,
            ..Default::default()
        };
        assert_eq!(MenuItemKind::PriceCliffPin.value(&settings), "children");
        assert_eq!(MenuItemKind::PriceCliffPin.activate(&mut settings), "all");
        assert_eq!(MenuItemKind::PriceCliffPin.activate(&mut settings), "off");
        assert!(MenuItemKind::PriceCliffPin.persists());

        let path = persist(&settings, tmp.path()).unwrap().unwrap();
        let loaded: PersistedSettings =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            loaded.compact_below_price_cliff,
            Some(mush_session::compact::PriceCliffPin::Off)
        );

        settings.preferred_providers = vec!["anthropic".into(), "openrouter".into()];
        let path = persist(&settings, tmp.path()).unwrap().unwrap();
        let loaded: PersistedSettings =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            loaded.preferred_providers,
            Some(vec!["anthropic".to_string(), "openrouter".to_string()])
        );
    }

    #[test]
    fn scope_round_trips() {
        for scope in [
            SettingsScope::Global,
            SettingsScope::Disabled,
            SettingsScope::Repo,
            SettingsScope::Session,
        ] {
            let parsed = SettingsScope::parse(scope.as_str()).unwrap();
            assert_eq!(parsed, scope);
        }
    }

    #[test]
    fn scope_parse_case_insensitive() {
        assert_eq!(SettingsScope::parse("GLOBAL"), Some(SettingsScope::Global));
        assert_eq!(SettingsScope::parse("  Repo  "), Some(SettingsScope::Repo));
    }

    #[test]
    fn persist_session_scope_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = ScopedSettings {
            scope: SettingsScope::Session,
            anthropic_betas: AnthropicBetas::default(),
            ..Default::default()
        };
        let result = persist(&settings, tmp.path()).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn persist_disabled_scope_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = ScopedSettings {
            scope: SettingsScope::Disabled,
            anthropic_betas: AnthropicBetas::default(),
            ..Default::default()
        };
        let err = persist(&settings, tmp.path()).unwrap_err();
        assert!(err.contains("disabled"));
    }

    #[test]
    fn persist_repo_writes_to_dotmush_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = ScopedSettings {
            scope: SettingsScope::Repo,
            anthropic_betas: AnthropicBetas {
                context_1m: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let path = persist(&settings, tmp.path()).unwrap().unwrap();
        assert!(path.ends_with(".mush/settings.toml"));
        let body = std::fs::read_to_string(&path).unwrap();
        let loaded: PersistedSettings = toml::from_str(&body).unwrap();
        assert!(!loaded.anthropic_betas.context_1m);
        assert!(loaded.anthropic_betas.effort);
    }

    #[test]
    fn the_saved_rlm_max_depth_and_the_betas_keep_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        save_rlm_max_depth_to(&path, 4).unwrap();
        let settings = ScopedSettings {
            scope: SettingsScope::Global,
            anthropic_betas: AnthropicBetas {
                context_1m: false,
                ..Default::default()
            },
            ..Default::default()
        };
        write_settings_file(&path, &settings).unwrap();
        save_rlm_max_depth_to(&path, 1).unwrap();

        let loaded: PersistedSettings =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.rlm_max_depth, Some(1));
        assert!(!loaded.anthropic_betas.context_1m);
    }
}
