//! hook system for running extensions in order
//!
//! the hook runner manages a set of extensions and invokes their
//! lifecycle methods in registration order.

use crate::types::*;
use mush_ai::types::Message;

/// manages and invokes extensions
pub struct HookRunner {
    extensions: Vec<Box<dyn Extension>>,
}

impl HookRunner {
    pub fn new() -> Self {
        Self { extensions: vec![] }
    }

    pub fn register(&mut self, ext: Box<dyn Extension>) {
        self.extensions.push(ext);
    }

    pub fn extensions(&self) -> &[Box<dyn Extension>] {
        &self.extensions
    }

    /// run discovery across all extensions, merging results
    pub fn discover(&self, ctx: &ExtensionContext) -> DiscoveredResources {
        let mut merged = DiscoveredResources::default();
        for ext in &self.extensions {
            let resources = ext.on_discover(ctx);
            merged
                .system_prompt_additions
                .extend(resources.system_prompt_additions);
            merged.tools.extend(resources.tools);
        }
        merged
    }

    /// run before-call transforms through all extensions in order
    pub fn before_call(
        &self,
        ctx: &ExtensionContext,
        messages: Vec<Message>,
        system_prompt: Option<String>,
    ) -> TransformResult {
        let mut result = TransformResult {
            messages,
            system_prompt,
        };
        for ext in &self.extensions {
            result = ext.on_before_call(ctx, result.messages, result.system_prompt);
        }
        result
    }

    /// notify all extensions that a turn completed
    pub fn turn_complete(&self, ctx: &ExtensionContext, messages: &[Message]) {
        for ext in &self.extensions {
            ext.on_turn_complete(ctx, messages);
        }
    }

    /// notify all extensions that the session is ending
    pub fn session_end(&self, ctx: &ExtensionContext) {
        for ext in &self.extensions {
            ext.on_session_end(ctx);
        }
    }
}

impl Default for HookRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PromptExt {
        meta: ExtensionMeta,
        addition: String,
    }

    impl Extension for PromptExt {
        fn meta(&self) -> &ExtensionMeta {
            &self.meta
        }

        fn on_discover(&self, _ctx: &ExtensionContext) -> DiscoveredResources {
            DiscoveredResources {
                system_prompt_additions: vec![self.addition.clone()],
                tools: vec![],
            }
        }
    }

    struct CounterExt {
        meta: ExtensionMeta,
        turn_count: Arc<AtomicUsize>,
    }

    impl Extension for CounterExt {
        fn meta(&self) -> &ExtensionMeta {
            &self.meta
        }

        fn on_turn_complete(&self, _ctx: &ExtensionContext, _messages: &[Message]) {
            self.turn_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn test_ctx() -> ExtensionContext {
        ExtensionContext {
            cwd: PathBuf::from("/tmp"),
            model_id: "test".into(),
            session_id: None,
        }
    }

    fn test_meta(name: &str) -> ExtensionMeta {
        ExtensionMeta {
            name: name.into(),
            description: "test".into(),
            path: PathBuf::from("/test"),
        }
    }

    #[test]
    fn discover_merges_all_extensions() {
        let mut runner = HookRunner::new();
        runner.register(Box::new(PromptExt {
            meta: test_meta("ext1"),
            addition: "from ext1".into(),
        }));
        runner.register(Box::new(PromptExt {
            meta: test_meta("ext2"),
            addition: "from ext2".into(),
        }));

        let resources = runner.discover(&test_ctx());
        assert_eq!(resources.system_prompt_additions.len(), 2);
    }

    #[test]
    fn turn_complete_notifies_all() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut runner = HookRunner::new();
        runner.register(Box::new(CounterExt {
            meta: test_meta("counter"),
            turn_count: counter.clone(),
        }));

        runner.turn_complete(&test_ctx(), &[]);
        runner.turn_complete(&test_ctx(), &[]);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn before_call_chains_transforms() {
        struct PrefixExt {
            meta: ExtensionMeta,
            prefix: String,
        }

        impl Extension for PrefixExt {
            fn meta(&self) -> &ExtensionMeta {
                &self.meta
            }

            fn on_before_call(
                &self,
                _ctx: &ExtensionContext,
                messages: Vec<Message>,
                system_prompt: Option<String>,
            ) -> TransformResult {
                let prompt = system_prompt.map(|p| format!("{}\n{}", self.prefix, p));
                TransformResult {
                    messages,
                    system_prompt: prompt,
                }
            }
        }

        let mut runner = HookRunner::new();
        runner.register(Box::new(PrefixExt {
            meta: test_meta("a"),
            prefix: "[A]".into(),
        }));
        runner.register(Box::new(PrefixExt {
            meta: test_meta("b"),
            prefix: "[B]".into(),
        }));

        let result = runner.before_call(&test_ctx(), vec![], Some("base".into()));
        // [B] should wrap [A] which wraps base
        let prompt = result.system_prompt.unwrap();
        assert!(prompt.starts_with("[B]"));
        assert!(prompt.contains("[A]"));
        assert!(prompt.contains("base"));
    }

    #[test]
    fn empty_runner_passes_through() {
        let runner = HookRunner::new();
        let result = runner.before_call(&test_ctx(), vec![], Some("unchanged".into()));
        assert_eq!(result.system_prompt.as_deref(), Some("unchanged"));
    }
}
