use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::nonpoison::Mutex;

pub const MAX_NOTE_CHARS: usize = 4_000;
pub const MAX_NOTES_CHARS: usize = 24_000;

pub struct Notes {
    path: Mutex<Option<PathBuf>>,
    memory: Mutex<String>,
}

impl Notes {
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            memory: Mutex::new(load(path.as_deref())),
            path: Mutex::new(path),
        })
    }

    pub fn rebind(&self, path: Option<PathBuf>) {
        let memory = load(path.as_deref());
        *self.path.lock() = path;
        *self.memory.lock() = memory;
    }

    #[must_use]
    pub fn read(&self) -> String {
        self.memory.lock().clone()
    }

    pub fn append(&self, text: &str) -> Result<String, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a note needs some text".into());
        }
        if text.chars().count() > MAX_NOTE_CHARS {
            return Err(format!("a note is at most {MAX_NOTE_CHARS} characters"));
        }
        let mut memory = self.memory.lock();
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
        let mut memory = self.memory.lock();
        *memory = text.trim().to_string();
        if !memory.is_empty() {
            memory.push('\n');
        }
        let snapshot = memory.clone();
        drop(memory);
        self.persist(&snapshot);
    }

    fn persist(&self, text: &str) {
        let Some(path) = self.path.lock().clone() else {
            return;
        };
        if text.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            crate::persist::write_private(&path, text);
        }
    }
}

fn load(path: Option<&Path>) -> String {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

pub use mush_session::notes::{extract, fingerprint, latest_fingerprint, message};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_append_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        let notes = Notes::new(Some(path.clone()));
        assert!(notes.read().is_empty());

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
        let reloaded = Notes::new(Some(path.clone()));
        assert_eq!(reloaded.read(), text);
        reloaded.replace("");
        assert!(!path.exists());
        assert!(reloaded.read().is_empty());
    }

    #[test]
    fn the_notes_message_roundtrips_its_fingerprint() {
        let notes = Notes::new(None);
        notes.append("the daemon lives in mush-daemon").unwrap();
        let text = notes.read();
        let carrier = message(&text);
        let mush_ai::types::Message::User(user) = &carrier else {
            panic!("{carrier:?}")
        };
        let (found, body) = extract(&user.text()).unwrap();
        assert_eq!(found, fingerprint(&text));
        assert!(body.ends_with("- the daemon lives in mush-daemon"));
        assert_eq!(
            latest_fingerprint(&[carrier]).as_deref(),
            Some(fingerprint(&text).as_str())
        );
        assert_eq!(latest_fingerprint(&[]), None);
        assert_ne!(fingerprint(&text), fingerprint("something else"));
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
