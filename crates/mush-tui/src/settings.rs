//! runtime settings: scope control and persistence helpers
//!
//! `SettingsScope` decides where runtime changes (made via `/settings`)
//! get persisted. `ScopedSettings` is a convenience pair of scope + betas
//! that the TUI holds and mutates as the user interacts with `/settings`

use mush_ai::types::AnthropicBetas;

/// scope for persisting settings changes made during a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsScope {
    /// changes persist to `~/.config/mush/settings.toml` (global defaults)
    Global,
    /// no runtime changes allowed; base config is authoritative
    Disabled,
    /// changes persist to `<repo>/.mush/settings.toml` (per-project)
    Repo,
    /// changes only apply to the current session, not persisted
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
}

/// active runtime settings held by the TUI
#[derive(Debug, Clone, Default)]
pub struct ScopedSettings {
    pub scope: SettingsScope,
    pub anthropic_betas: AnthropicBetas,
}

impl SettingsScope {
    /// return the next variant when cycling via space/enter in the menu
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

/// menu item kinds surfaced by the /settings overlay
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItemKind {
    Scope,
    ContextOneM,
    Effort,
    ContextManagement,
    RedactThinking,
    Advisor,
    AdvancedToolUse,
}

impl MenuItemKind {
    #[must_use]
    pub fn all() -> &'static [MenuItemKind] {
        &[
            Self::Scope,
            Self::ContextOneM,
            Self::Effort,
            Self::ContextManagement,
            Self::RedactThinking,
            Self::Advisor,
            Self::AdvancedToolUse,
        ]
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::ContextOneM => "context_1m",
            Self::Effort => "effort",
            Self::ContextManagement => "context_management",
            Self::RedactThinking => "redact_thinking",
            Self::Advisor => "advisor",
            Self::AdvancedToolUse => "advanced_tool_use",
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Scope => "where /settings changes get persisted",
            Self::ContextOneM => "context-1m-2025-08-07: 1M context on claude models",
            Self::Effort => "effort-2025-11-24: send output_config.effort",
            Self::ContextManagement => "context-management-2025-06-27: server-side edits",
            Self::RedactThinking => "redact-thinking-2026-02-12: allow thinking redaction",
            Self::Advisor => "advisor-tool-2026-03-01: advisor tool (not yet supported)",
            Self::AdvancedToolUse => {
                "advanced-tool-use-2025-11-20: advanced tool-use (experimental)"
            }
        }
    }

    /// toggle or cycle the underlying value. returns the new display value
    pub fn activate(self, settings: &mut ScopedSettings) -> String {
        match self {
            Self::Scope => {
                settings.scope = settings.scope.cycle_next();
                settings.scope.as_str().to_string()
            }
            Self::ContextOneM => {
                settings.anthropic_betas.context_1m = !settings.anthropic_betas.context_1m;
                bool_str(settings.anthropic_betas.context_1m).to_string()
            }
            Self::Effort => {
                settings.anthropic_betas.effort = !settings.anthropic_betas.effort;
                bool_str(settings.anthropic_betas.effort).to_string()
            }
            Self::ContextManagement => {
                settings.anthropic_betas.context_management =
                    !settings.anthropic_betas.context_management;
                bool_str(settings.anthropic_betas.context_management).to_string()
            }
            Self::RedactThinking => {
                settings.anthropic_betas.redact_thinking =
                    !settings.anthropic_betas.redact_thinking;
                bool_str(settings.anthropic_betas.redact_thinking).to_string()
            }
            Self::Advisor => {
                settings.anthropic_betas.advisor = !settings.anthropic_betas.advisor;
                bool_str(settings.anthropic_betas.advisor).to_string()
            }
            Self::AdvancedToolUse => {
                settings.anthropic_betas.advanced_tool_use =
                    !settings.anthropic_betas.advanced_tool_use;
                bool_str(settings.anthropic_betas.advanced_tool_use).to_string()
            }
        }
    }

    /// current display value for the given settings (without mutating)
    #[must_use]
    pub fn value(self, settings: &ScopedSettings) -> String {
        match self {
            Self::Scope => settings.scope.as_str().to_string(),
            Self::ContextOneM => bool_str(settings.anthropic_betas.context_1m).to_string(),
            Self::Effort => bool_str(settings.anthropic_betas.effort).to_string(),
            Self::ContextManagement => {
                bool_str(settings.anthropic_betas.context_management).to_string()
            }
            Self::RedactThinking => bool_str(settings.anthropic_betas.redact_thinking).to_string(),
            Self::Advisor => bool_str(settings.anthropic_betas.advisor).to_string(),
            Self::AdvancedToolUse => {
                bool_str(settings.anthropic_betas.advanced_tool_use).to_string()
            }
        }
    }
}

fn bool_str(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// ui state for the /settings floating menu
#[derive(Debug, Clone, Default)]
pub struct SettingsMenuState {
    /// index of the currently selected row
    pub selected: usize,
    /// scroll offset for long menus
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

/// serialisable form of `ScopedSettings` for persistence
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedSettings {
    pub anthropic_betas: AnthropicBetas,
}

impl From<&ScopedSettings> for PersistedSettings {
    fn from(s: &ScopedSettings) -> Self {
        Self {
            anthropic_betas: s.anthropic_betas.clone(),
        }
    }
}

/// persist the current settings according to scope.
/// returns Ok(Some(path)) on successful write, Ok(None) if scope is
/// Session (no persistence), or Err on disabled/io failure
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let persisted = PersistedSettings::from(settings);
    let body =
        toml::to_string_pretty(&persisted).map_err(|e| format!("serialise settings: {e}"))?;
    std::fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
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
        };
        let path = persist(&settings, tmp.path()).unwrap().unwrap();
        assert!(path.ends_with(".mush/settings.toml"));
        let body = std::fs::read_to_string(&path).unwrap();
        let loaded: PersistedSettings = toml::from_str(&body).unwrap();
        assert!(!loaded.anthropic_betas.context_1m);
        assert!(loaded.anthropic_betas.effort); // other defaults preserved
    }
}
