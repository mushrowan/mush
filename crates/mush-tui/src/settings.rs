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
