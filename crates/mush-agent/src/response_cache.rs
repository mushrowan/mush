use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mush_ai::types::Message;

#[derive(Debug, Clone)]
struct CacheEntry {
    response: Message,
    cached_at: Instant,
}

#[derive(Clone)]
pub struct ResponseCache {
    entries: Arc<Mutex<HashMap<u64, CacheEntry>>>,
    ttl: Duration,
    max_entries: usize,
}

impl ResponseCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            ttl,
            max_entries,
        }
    }

    pub fn key(model_id: &str, system_prompt: Option<&str>, messages: &[Message]) -> u64 {
        let mut hasher = DefaultHasher::new();
        model_id.hash(&mut hasher);
        system_prompt.hash(&mut hasher);
        messages.len().hash(&mut hasher);
        for msg in messages {
            if let Ok(bytes) = serde_json::to_vec(msg) {
                bytes.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    pub fn get(&self, key: u64) -> Option<Message> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get(&key) {
            if entry.cached_at.elapsed() < self.ttl {
                return Some(entry.response.clone());
            }
            entries.remove(&key);
        }
        None
    }

    pub fn put(&self, key: u64, response: Message) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());

        if entries.len() >= self.max_entries {
            let now = Instant::now();
            entries.retain(|_, e| now.duration_since(e.cached_at) < self.ttl);
        }

        if entries.len() >= self.max_entries
            && let Some(&oldest_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.cached_at)
                .map(|(k, _)| k)
        {
            entries.remove(&oldest_key);
        }

        entries.insert(
            key,
            CacheEntry {
                response,
                cached_at: Instant::now(),
            },
        );
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::*;

    fn user_msg(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: text.into(),
                })])
                .model("test".into())
                .provider(Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[test]
    fn cache_stores_and_retrieves() {
        let cache = ResponseCache::new(Duration::from_secs(60), 100);
        let msgs = [user_msg("hello")];
        let key = ResponseCache::key("model", Some("prompt"), &msgs);

        assert!(cache.get(key).is_none());

        cache.put(key, assistant_msg("hi there"));
        assert!(cache.get(key).is_some());
    }

    #[test]
    fn different_messages_different_keys() {
        let k1 = ResponseCache::key("model", None, &[user_msg("hello")]);
        let k2 = ResponseCache::key("model", None, &[user_msg("world")]);
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_models_different_keys() {
        let msgs = [user_msg("hello")];
        let k1 = ResponseCache::key("model-a", None, &msgs);
        let k2 = ResponseCache::key("model-b", None, &msgs);
        assert_ne!(k1, k2);
    }

    #[test]
    fn same_inputs_same_key() {
        let msgs = [user_msg("hello")];
        let k1 = ResponseCache::key("model", Some("sys"), &msgs);
        let k2 = ResponseCache::key("model", Some("sys"), &msgs);
        assert_eq!(k1, k2);
    }

    #[test]
    fn expired_entries_not_returned() {
        let cache = ResponseCache::new(Duration::from_millis(1), 100);
        let key = ResponseCache::key("m", None, &[]);

        cache.put(key, assistant_msg("cached"));
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.get(key).is_none());
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let cache = ResponseCache::new(Duration::from_secs(60), 2);

        let k1 = ResponseCache::key("m", None, &[user_msg("a")]);
        let k2 = ResponseCache::key("m", None, &[user_msg("b")]);
        let k3 = ResponseCache::key("m", None, &[user_msg("c")]);

        cache.put(k1, assistant_msg("r1"));
        cache.put(k2, assistant_msg("r2"));
        assert_eq!(cache.len(), 2);

        cache.put(k3, assistant_msg("r3"));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(k1).is_none());
        assert!(cache.get(k3).is_some());
    }

    #[test]
    fn clear_removes_all() {
        let cache = ResponseCache::new(Duration::from_secs(60), 100);
        cache.put(1, assistant_msg("a"));
        cache.put(2, assistant_msg("b"));
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn key_via_serde_is_deterministic() {
        let msgs = [user_msg("hello"), user_msg("world")];
        let k1 = ResponseCache::key("model", Some("sys"), &msgs);
        let k2 = ResponseCache::key("model", Some("sys"), &msgs);
        assert_eq!(k1, k2);

        let k3 = ResponseCache::key("model", Some("sys"), &[user_msg("hello")]);
        assert_ne!(k1, k3);
    }
}
