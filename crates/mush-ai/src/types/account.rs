use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, derive_more::Display, Serialize, Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct AccountLabel(String);

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum AccountLabelError {
    #[error("account label cannot be empty")]
    Empty,
    #[error("account label '{0}' is longer than {max} characters", max = AccountLabel::MAX_LEN)]
    TooLong(String),
    #[error("account label '{0}' may only use lowercase letters, digits, '-' and '_'")]
    InvalidChar(String),
}

impl AccountLabel {
    pub const MAX_LEN: usize = 32;
    pub const DEFAULT: &str = "default";

    pub fn parse(label: &str) -> Result<Self, AccountLabelError> {
        let label = label.trim();
        if label.is_empty() {
            return Err(AccountLabelError::Empty);
        }
        if label.chars().count() > Self::MAX_LEN {
            return Err(AccountLabelError::TooLong(label.to_string()));
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(AccountLabelError::InvalidChar(label.to_string()));
        }
        Ok(Self(label.to_string()))
    }

    #[must_use]
    pub fn default_label() -> Self {
        Self(Self::DEFAULT.to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AccountLabel {
    type Error = AccountLabelError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<AccountLabel> for String {
    fn from(label: AccountLabel) -> Self {
        label.0
    }
}

impl std::str::FromStr for AccountLabel {
    type Err = AccountLabelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionAccounts(BTreeMap<String, AccountLabel>);

impl SessionAccounts {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn get(&self, oauth_provider: &str) -> Option<&AccountLabel> {
        self.0.get(oauth_provider)
    }

    pub fn set(&mut self, oauth_provider: &str, label: Option<AccountLabel>) {
        match label {
            Some(label) => {
                self.0.insert(oauth_provider.to_string(), label);
            }
            None => {
                self.0.remove(oauth_provider);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &AccountLabel)> {
        self.0
            .iter()
            .map(|(provider, label)| (provider.as_str(), label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_short_lowercase_words() {
        assert_eq!(AccountLabel::parse(" work ").unwrap().as_str(), "work");
        assert_eq!(
            AccountLabel::parse("side_project-2").unwrap().as_str(),
            "side_project-2"
        );
        assert_eq!(AccountLabel::parse(""), Err(AccountLabelError::Empty));
        assert!(matches!(
            AccountLabel::parse("Work"),
            Err(AccountLabelError::InvalidChar(_))
        ));
        assert!(matches!(
            AccountLabel::parse("two words"),
            Err(AccountLabelError::InvalidChar(_))
        ));
        assert!(matches!(
            AccountLabel::parse(&"a".repeat(33)),
            Err(AccountLabelError::TooLong(_))
        ));
    }

    #[test]
    fn a_bad_label_does_not_deserialise() {
        assert!(serde_json::from_str::<AccountLabel>("\"work\"").is_ok());
        assert!(serde_json::from_str::<AccountLabel>("\"Not Ok\"").is_err());
    }

    #[test]
    fn session_accounts_are_keyed_by_provider() {
        let mut accounts = SessionAccounts::default();
        accounts.set("anthropic", Some(AccountLabel::parse("work").unwrap()));
        assert_eq!(accounts.get("anthropic").unwrap().as_str(), "work");
        assert!(accounts.get("openai-codex").is_none());
        let json = serde_json::to_string(&accounts).unwrap();
        assert_eq!(json, r#"{"anthropic":"work"}"#);
        accounts.set("anthropic", None);
        assert!(accounts.is_empty());
    }
}
