//! content types for user, assistant, and tool result messages

use serde::{Deserialize, Deserializer, Serialize};

use super::newtypes::{ToolCallId, ToolName};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextContent {
    pub text: String,
}

/// thinking block in an assistant message
#[derive(Debug, Clone, PartialEq)]
pub enum ThinkingContent {
    /// normal thinking with text and optional signature for multi-turn continuity
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    /// redacted thinking, opaque signature only
    Redacted { data: String },
}

impl ThinkingContent {
    /// the thinking text, or a placeholder for redacted blocks
    pub fn text(&self) -> &str {
        match self {
            Self::Thinking { thinking, .. } => thinking,
            Self::Redacted { .. } => "[reasoning redacted]",
        }
    }

    /// mutable access to the thinking text (only for non-redacted)
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Thinking { thinking, .. } => Some(thinking),
            Self::Redacted { .. } => None,
        }
    }

    /// the signature, if any
    pub fn signature(&self) -> Option<&str> {
        match self {
            Self::Thinking { signature, .. } => signature.as_deref(),
            Self::Redacted { data } => Some(data),
        }
    }

    /// mutable access to the signature slot (only for non-redacted)
    pub fn signature_mut(&mut self) -> Option<&mut Option<String>> {
        match self {
            Self::Thinking { signature, .. } => Some(signature),
            Self::Redacted { .. } => None,
        }
    }

    pub fn is_redacted(&self) -> bool {
        matches!(self, Self::Redacted { .. })
    }
}

impl Serialize for ThinkingContent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Thinking {
                thinking,
                signature,
            } => {
                let len = 2 + signature.is_some() as usize;
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("thinking", thinking)?;
                map.serialize_entry("redacted", &false)?;
                if let Some(sig) = signature {
                    map.serialize_entry("signature", sig)?;
                }
                map.end()
            }
            Self::Redacted { data } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("thinking", "[reasoning redacted]")?;
                map.serialize_entry("redacted", &true)?;
                map.serialize_entry("signature", data)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ThinkingContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            thinking: String,
            #[serde(default)]
            redacted: bool,
            signature: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.redacted {
            Ok(Self::Redacted {
                data: raw.signature.unwrap_or_default(),
            })
        } else {
            Ok(Self::Thinking {
                thinking: raw.thinking,
                signature: raw.signature,
            })
        }
    }
}

/// supported image mime types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageMimeType {
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/gif")]
    Gif,
    #[serde(rename = "image/webp")]
    Webp,
}

impl ImageMimeType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// parse from file extension (defaults to png)
    #[must_use]
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "jpg" | "jpeg" => Self::Jpeg,
            "gif" => Self::Gif,
            "webp" => Self::Webp,
            _ => Self::Png,
        }
    }
}

impl std::fmt::Display for ImageMimeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageContent {
    /// base64 encoded image data
    pub data: String,
    pub mime_type: ImageMimeType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: ToolName,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<UserContentPart>),
}

impl UserContent {
    /// extract the text content, joining text parts if multipart
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    UserContentPart::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentPart {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContentPart {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContentPart {
    Text(TextContent),
    Image(ImageContent),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_content_serde_roundtrip() {
        let normal = ThinkingContent::Thinking {
            thinking: "let me think".into(),
            signature: Some("sig123".into()),
        };
        let json = serde_json::to_string(&normal).unwrap();
        let back: ThinkingContent = serde_json::from_str(&json).unwrap();
        assert_eq!(normal, back);
        assert!(!back.is_redacted());
        assert_eq!(back.text(), "let me think");
        assert_eq!(back.signature(), Some("sig123"));
    }

    #[test]
    fn thinking_content_redacted_serde_roundtrip() {
        let redacted = ThinkingContent::Redacted {
            data: "opaque_data".into(),
        };
        let json = serde_json::to_string(&redacted).unwrap();
        let back: ThinkingContent = serde_json::from_str(&json).unwrap();
        assert_eq!(redacted, back);
        assert!(back.is_redacted());
        assert_eq!(back.text(), "[reasoning redacted]");
        assert_eq!(back.signature(), Some("opaque_data"));
    }

    #[test]
    fn thinking_content_deserialises_legacy_format() {
        let json = r#"{"thinking": "hello", "signature": "sig", "redacted": false}"#;
        let tc: ThinkingContent = serde_json::from_str(json).unwrap();
        assert!(!tc.is_redacted());
        assert_eq!(tc.text(), "hello");

        let json = r#"{"thinking": "[reasoning redacted]", "signature": "data", "redacted": true}"#;
        let tc: ThinkingContent = serde_json::from_str(json).unwrap();
        assert!(tc.is_redacted());
        assert_eq!(tc.signature(), Some("data"));

        let json = r#"{"thinking": "no redacted field"}"#;
        let tc: ThinkingContent = serde_json::from_str(json).unwrap();
        assert!(!tc.is_redacted());
        assert_eq!(tc.text(), "no redacted field");
    }
}
