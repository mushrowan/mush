//! extension types and trait definitions
//!
//! extensions can hook into the agent lifecycle, register custom tools,
//! modify system prompts, and add commands.

use std::path::PathBuf;

use mush_agent::tool::AgentTool;
use mush_ai::types::Message;

/// metadata about a loaded extension
#[derive(Debug, Clone)]
pub struct ExtensionMeta {
    /// unique name for this extension
    pub name: String,
    /// human-readable description
    pub description: String,
    /// path the extension was loaded from
    pub path: PathBuf,
}

/// context available to extensions during lifecycle events
pub struct ExtensionContext {
    pub cwd: PathBuf,
    pub model_id: String,
    pub session_id: Option<String>,
}

/// result of discovering resources (system prompt additions, tools, etc)
#[derive(Default)]
pub struct DiscoveredResources {
    /// additional system prompt content to append
    pub system_prompt_additions: Vec<String>,
    /// additional tool definitions (extensions can provide tools)
    pub tools: Vec<Box<dyn AgentTool>>,
}

/// result of a context transform (modify messages before sending to LLM)
pub struct TransformResult {
    pub messages: Vec<Message>,
    pub system_prompt: Option<String>,
}

/// trait that extensions implement
///
/// all methods have default no-op implementations so extensions
/// only need to override what they care about.
pub trait Extension: Send + Sync {
    /// extension metadata
    fn meta(&self) -> &ExtensionMeta;

    /// called when a session starts, to discover resources like
    /// system prompt additions, custom tools, etc
    fn on_discover(&self, _ctx: &ExtensionContext) -> DiscoveredResources {
        DiscoveredResources::default()
    }

    /// called before each LLM call to transform the context
    /// (e.g. inject skill instructions, modify system prompt)
    fn on_before_call(
        &self,
        _ctx: &ExtensionContext,
        messages: Vec<Message>,
        system_prompt: Option<String>,
    ) -> TransformResult {
        TransformResult {
            messages,
            system_prompt,
        }
    }

    /// called after the agent completes a turn
    fn on_turn_complete(&self, _ctx: &ExtensionContext, _messages: &[Message]) {}

    /// called when the session ends
    fn on_session_end(&self, _ctx: &ExtensionContext) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestExt {
        meta: ExtensionMeta,
    }

    impl Extension for TestExt {
        fn meta(&self) -> &ExtensionMeta {
            &self.meta
        }

        fn on_discover(&self, _ctx: &ExtensionContext) -> DiscoveredResources {
            DiscoveredResources {
                system_prompt_additions: vec!["test extension active".into()],
                tools: vec![],
            }
        }
    }

    #[test]
    fn extension_discover_resources() {
        let ext = TestExt {
            meta: ExtensionMeta {
                name: "test".into(),
                description: "test extension".into(),
                path: PathBuf::from("/test"),
            },
        };

        let ctx = ExtensionContext {
            cwd: PathBuf::from("/tmp"),
            model_id: "test".into(),
            session_id: None,
        };

        let resources = ext.on_discover(&ctx);
        assert_eq!(resources.system_prompt_additions.len(), 1);
        assert!(resources.system_prompt_additions[0].contains("test extension"));
    }

    #[test]
    fn default_before_call_passes_through() {
        let ext = TestExt {
            meta: ExtensionMeta {
                name: "test".into(),
                description: "test".into(),
                path: PathBuf::from("/test"),
            },
        };

        let ctx = ExtensionContext {
            cwd: PathBuf::from("/tmp"),
            model_id: "test".into(),
            session_id: None,
        };

        let result = ext.on_before_call(&ctx, vec![], Some("original prompt".into()));
        assert_eq!(result.system_prompt.as_deref(), Some("original prompt"));
        assert!(result.messages.is_empty());
    }
}
