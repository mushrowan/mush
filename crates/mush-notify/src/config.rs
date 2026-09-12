use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Disabled,
    DryRun,
    Send,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[schemars(description = "user-only opt-in, disabled by default")]
    pub mode: Mode,
    #[schemars(description = "explicit account number, empty by default")]
    pub account: String,
    #[schemars(description = "explicit recipient numbers, empty by default")]
    pub recipients: Vec<String>,
    #[schemars(description = "absolute private unix socket, no default discovery")]
    pub socket: Option<PathBuf>,
    #[schemars(description = "completion grace in seconds, default 60")]
    pub grace_seconds: u64,
    #[schemars(description = "minimum confirmed idle seconds, default 300")]
    pub idle_seconds: u64,
    #[schemars(description = "maximum activity snapshot age in seconds, default 5")]
    pub freshness_seconds: u64,
    #[schemars(description = "minimum attempt interval in seconds, default 60")]
    pub rate_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Disabled,
            account: String::new(),
            recipients: Vec::new(),
            socket: None,
            grace_seconds: 60,
            idle_seconds: 300,
            freshness_seconds: 5,
            rate_seconds: 60,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("configure an account and recipient numbers")]
    Address,
    #[error("configure an absolute private unix socket")]
    Socket,
    #[error("notification timing is out of range")]
    Timing,
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.mode == Mode::Disabled {
            return Ok(());
        }
        if !number(&self.account)
            || self.recipients.is_empty()
            || self.recipients.len() > 4
            || self.recipients.iter().any(|s| !number(s))
        {
            return Err(ConfigError::Address);
        }
        if !self.socket.as_ref().is_some_and(|s| {
            s.is_absolute()
                && s.as_os_str().len() < 104
                && s.components().all(|c| {
                    matches!(
                        c,
                        std::path::Component::RootDir | std::path::Component::Normal(_)
                    )
                })
        }) {
            return Err(ConfigError::Socket);
        }
        if !(1..=3600).contains(&self.grace_seconds)
            || !(1..=86400).contains(&self.idle_seconds)
            || !(1..=5).contains(&self.freshness_seconds)
            || !(60..=86400).contains(&self.rate_seconds)
        {
            return Err(ConfigError::Timing);
        }
        Ok(())
    }
}

fn number(s: &str) -> bool {
    s.strip_prefix('+').is_some_and(|n| {
        (7..=15).contains(&n.len()) && !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn configured() -> Config {
        Config {
            mode: Mode::DryRun,
            account: "+12025550100".into(),
            recipients: vec!["+12025550101".into()],
            socket: Some("/run/user/1000/notify/socket".into()),
            ..Config::default()
        }
    }

    #[test]
    fn disabled_and_no_discovery_by_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.mode, Mode::Disabled);
        assert!(config.account.is_empty());
        assert!(config.recipients.is_empty());
        assert!(config.socket.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn reject_commands_modes_and_unconfigured_targets() {
        for text in ["mode = 'always'", "command = 'curl'", "message = 'private'"] {
            assert!(toml::from_str::<Config>(text).is_err());
        }
        assert_eq!(
            Config {
                mode: Mode::Send,
                ..Config::default()
            }
            .validate(),
            Err(ConfigError::Address)
        );
        let mut c = configured();
        assert!(c.validate().is_ok());
        for path in ["http://localhost:8080", "socket", "/run/../socket"] {
            c.socket = Some(path.into());
            assert_eq!(c.validate(), Err(ConfigError::Socket));
        }
        c = configured();
        c.recipients = vec!["u:lookup.000".into()];
        assert_eq!(c.validate(), Err(ConfigError::Address));
        c = configured();
        c.rate_seconds = 0;
        assert_eq!(c.validate(), Err(ConfigError::Timing));
    }

    #[test]
    fn schema_exposes_defaults_and_closed_modes() {
        let schema = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
        assert_eq!(schema["properties"]["mode"]["default"], "disabled");
        assert_eq!(
            schema["properties"]["recipients"]["default"],
            serde_json::json!([])
        );
        assert_eq!(schema["properties"]["grace_seconds"]["default"], 60);
        assert_eq!(schema["additionalProperties"], false);
    }
}
