use std::path::PathBuf;

use mush_agent::tool::ToolRegistry;
use mush_ai::types::{Message, ModelId, SessionId};

#[derive(Debug, Clone)]
pub struct ExtensionMeta {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

pub struct ExtensionContext {
    pub cwd: PathBuf,
    pub model_id: ModelId,
    pub session_id: Option<SessionId>,
}

#[derive(Default)]
pub struct DiscoveredResources {
    pub system_prompt_additions: Vec<String>,
    pub tools: ToolRegistry,
}

pub struct TransformResult {
    pub messages: Vec<Message>,
    pub system_prompt: Option<String>,
}

pub trait Extension: Send + Sync {
    fn meta(&self) -> &ExtensionMeta;

    fn on_discover(&self, _ctx: &ExtensionContext) -> DiscoveredResources {
        DiscoveredResources::default()
    }

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

    fn on_turn_complete(&self, _ctx: &ExtensionContext, _messages: &[Message]) {}

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
                tools: ToolRegistry::new(),
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
