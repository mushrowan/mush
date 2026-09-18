use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const MAX_NOTE_CHARS: usize = 4_000;
pub const MAX_NOTES_CHARS: usize = 24_000;

pub struct Notes {
    path: Mutex<Option<PathBuf>>,
    memory: Mutex<String>,
}

impl Notes {
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Arc<Self> {
        let memory = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        Arc::new(Self {
            path: Mutex::new(path),
            memory: Mutex::new(memory),
        })
    }

    pub fn rebind(&self, path: Option<PathBuf>) {
        let memory = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        *self
            .path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
        *self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = memory;
    }

    #[must_use]
    pub fn read(&self) -> String {
        self.memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn append(&self, text: &str) -> Result<String, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a note needs some text".into());
        }
        if text.chars().count() > MAX_NOTE_CHARS {
            return Err(format!("a note is at most {MAX_NOTE_CHARS} characters"));
        }
        let mut memory = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if memory.chars().count() + text.chars().count() > MAX_NOTES_CHARS {
            return Err(format!(
                "the notes are full ({MAX_NOTES_CHARS} characters); clear or condense them first"
            ));
        }
        if !memory.is_empty() && !memory.ends_with('\n') {
            memory.push('\n');
        }
        memory.push_str("- ");
        memory.push_str(text);
        memory.push('\n');
        let snapshot = memory.clone();
        drop(memory);
        self.persist(&snapshot);
        Ok(snapshot)
    }

    pub fn replace(&self, text: &str) {
        let mut memory = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *memory = text.trim().to_string();
        if !memory.is_empty() {
            memory.push('\n');
        }
        let snapshot = memory.clone();
        drop(memory);
        self.persist(&snapshot);
    }

    fn persist(&self, text: &str) {
        let path = self
            .path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(path) = path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if text.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            let _ = mush_ai::private_io::write_private(&path, text.as_bytes());
        }
    }

    #[must_use]
    pub fn dynamic_context(self: &Arc<Self>) -> mush_agent::DynamicContext {
        let notes = Arc::clone(self);
        Arc::new(move || {
            let text = notes.read();
            (!text.trim().is_empty()).then(|| {
                format!(
                    "# Notes\n\nNotes you kept earlier in this session with `rlm.note()`. They \
                     survive compaction and kernel restarts.\n\n{}",
                    text.trim_end()
                )
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_append_persist_and_feed_the_system_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        let notes = Notes::new(Some(path.clone()));
        let context = notes.dynamic_context();
        assert!(context().is_none());

        notes
            .append("tests run with env -u MUSH_KERNEL_PYTHON")
            .unwrap();
        notes.append("  the daemon lives in mush-daemon  ").unwrap();
        assert!(notes.append("   ").is_err());

        let text = notes.read();
        assert_eq!(
            text,
            "- tests run with env -u MUSH_KERNEL_PYTHON\n- the daemon lives in mush-daemon\n"
        );
        assert!(context().unwrap().starts_with("# Notes"));
        assert!(
            context()
                .unwrap()
                .ends_with("- the daemon lives in mush-daemon")
        );

        let reloaded = Notes::new(Some(path.clone()));
        assert_eq!(reloaded.read(), text);
        reloaded.replace("");
        assert!(!path.exists());
        assert!(reloaded.dynamic_context()().is_none());
    }

    #[test]
    fn notes_have_a_ceiling() {
        let notes = Notes::new(None);
        assert!(notes.append(&"x".repeat(MAX_NOTE_CHARS + 1)).is_err());
        for _ in 0..(MAX_NOTES_CHARS / MAX_NOTE_CHARS) {
            let _ = notes.append(&"y".repeat(MAX_NOTE_CHARS - 10));
        }
        assert!(notes.append(&"z".repeat(100)).unwrap_err().contains("full"));
    }
}
