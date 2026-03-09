use std::str::FromStr;

use serde::Deserialize;
use thiserror::Error;

pub const KEYBOARD_ENHANCEMENT_ENV: &str = "MUSH_TUI_KEYBOARD_ENHANCEMENT";
pub const MOUSE_TRACKING_ENV: &str = "MUSH_TUI_MOUSE_TRACKING";
pub const IMAGE_PROBE_ENV: &str = "MUSH_TUI_IMAGE_PROBE";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseTerminalPolicyError {
    #[error("invalid keyboard enhancement mode: {0}")]
    KeyboardEnhancement(String),
    #[error("invalid mouse tracking mode: {0}")]
    MouseTracking(String),
    #[error("invalid image probe mode: {0}")]
    ImageProbe(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyboardEnhancementMode {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

impl KeyboardEnhancementMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "enabled" | "enable" | "on" | "true" | "1" => Some(Self::Enabled),
            "disabled" | "disable" | "off" | "false" | "0" => Some(Self::Disabled),
            _ => None,
        }
    }
}

impl FromStr for KeyboardEnhancementMode {
    type Err = ParseTerminalPolicyError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
            .ok_or_else(|| ParseTerminalPolicyError::KeyboardEnhancement(raw.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseTrackingMode {
    #[default]
    Minimal,
    Disabled,
}

impl MouseTrackingMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Disabled => "disabled",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "minimal" | "enabled" | "enable" | "on" | "true" | "1" => Some(Self::Minimal),
            "disabled" | "disable" | "off" | "false" | "0" | "none" => Some(Self::Disabled),
            _ => None,
        }
    }
}

impl FromStr for MouseTrackingMode {
    type Err = ParseTerminalPolicyError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw).ok_or_else(|| ParseTerminalPolicyError::MouseTracking(raw.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProbeMode {
    #[default]
    Auto,
    Disabled,
}

impl ImageProbeMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Disabled => "disabled",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "enabled" | "enable" | "on" | "true" | "1" => Some(Self::Auto),
            "disabled" | "disable" | "off" | "false" | "0" | "none" => Some(Self::Disabled),
            _ => None,
        }
    }
}

impl FromStr for ImageProbeMode {
    type Err = ParseTerminalPolicyError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw).ok_or_else(|| ParseTerminalPolicyError::ImageProbe(raw.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalPolicyOverrides {
    pub keyboard_enhancement: Option<KeyboardEnhancementMode>,
    pub mouse_tracking: Option<MouseTrackingMode>,
    pub image_probe: Option<ImageProbeMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct TerminalPolicy {
    pub keyboard_enhancement: KeyboardEnhancementMode,
    pub mouse_tracking: MouseTrackingMode,
    pub image_probe: ImageProbeMode,
}

impl TerminalPolicy {
    #[must_use]
    pub fn with_overrides(self, overrides: TerminalPolicyOverrides) -> Self {
        Self {
            keyboard_enhancement: overrides
                .keyboard_enhancement
                .unwrap_or(self.keyboard_enhancement),
            mouse_tracking: overrides.mouse_tracking.unwrap_or(self.mouse_tracking),
            image_probe: overrides.image_probe.unwrap_or(self.image_probe),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_current_terminal_behaviour() {
        let policy = TerminalPolicy::default();
        assert_eq!(policy.keyboard_enhancement, KeyboardEnhancementMode::Auto);
        assert_eq!(policy.mouse_tracking, MouseTrackingMode::Minimal);
        assert_eq!(policy.image_probe, ImageProbeMode::Auto);
    }

    #[test]
    fn from_str_accepts_cli_and_env_aliases() {
        assert_eq!(
            KeyboardEnhancementMode::from_str("off").unwrap(),
            KeyboardEnhancementMode::Disabled
        );
        assert_eq!(
            MouseTrackingMode::from_str("true").unwrap(),
            MouseTrackingMode::Minimal
        );
        assert_eq!(
            ImageProbeMode::from_str("none").unwrap(),
            ImageProbeMode::Disabled
        );
    }

    #[test]
    fn overrides_only_replace_requested_fields() {
        let policy = TerminalPolicy::default().with_overrides(TerminalPolicyOverrides {
            keyboard_enhancement: Some(KeyboardEnhancementMode::Disabled),
            image_probe: Some(ImageProbeMode::Disabled),
            ..TerminalPolicyOverrides::default()
        });

        assert_eq!(
            policy.keyboard_enhancement,
            KeyboardEnhancementMode::Disabled
        );
        assert_eq!(policy.mouse_tracking, MouseTrackingMode::Minimal);
        assert_eq!(policy.image_probe, ImageProbeMode::Disabled);
    }
}
