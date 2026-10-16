use std::sync::Arc;

use super::{InputBuffer, PendingImage};

const UNDO_LEVELS: usize = 1_000;

#[derive(Debug, Clone, Default)]
struct Snapshot {
    text: String,
    images: Arc<[PendingImage]>,
    cursor: usize,
}

#[derive(Debug, Default)]
pub(crate) struct History {
    past: Vec<Snapshot>,
    future: Vec<Snapshot>,
    settled: Snapshot,
}

impl InputBuffer {
    pub(crate) fn checkpoint(&mut self) {
        let settled = &mut self.history.settled;
        let images_held = same_images(&settled.images, &self.images);
        if images_held && settled.text == self.text {
            settled.cursor = self.cursor;
            return;
        }
        let images = if images_held {
            Arc::clone(&settled.images)
        } else {
            Arc::from(self.images.as_slice())
        };
        let now = Snapshot {
            text: self.text.clone(),
            images,
            cursor: self.cursor,
        };
        let before = std::mem::replace(&mut self.history.settled, now);
        self.history.past.push(before);
        if self.history.past.len() > UNDO_LEVELS {
            self.history.past.remove(0);
        }
        self.history.future.clear();
    }

    pub(crate) fn undo(&mut self) -> bool {
        self.checkpoint();
        let Some(before) = self.history.past.pop() else {
            return false;
        };
        let now = std::mem::replace(&mut self.history.settled, before);
        self.history.future.push(now);
        self.restore();
        true
    }

    pub(crate) fn redo(&mut self) -> bool {
        self.checkpoint();
        let Some(after) = self.history.future.pop() else {
            return false;
        };
        let now = std::mem::replace(&mut self.history.settled, after);
        self.history.past.push(now);
        self.restore();
        true
    }

    pub(crate) fn forget_history(&mut self) {
        self.history = History::default();
    }

    fn restore(&mut self) {
        let settled = &self.history.settled;
        self.text.clone_from(&settled.text);
        self.images = settled.images.to_vec();
        self.anchor = None;
        let mut cursor = settled.cursor.min(self.text.len());
        while !self.text.is_char_boundary(cursor) {
            cursor -= 1;
        }
        self.cursor = cursor;
    }
}

fn same_images(a: &[PendingImage], b: &[PendingImage]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.mime_type == b.mime_type && a.dimensions == b.dimensions && a.data == b.data
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> InputBuffer {
        let mut input = InputBuffer::new();
        input.insert_str(text);
        input
    }

    #[test]
    fn undo_goes_back_to_the_last_checkpoint() {
        let mut input = buffer("hello");
        input.checkpoint();
        input.insert_str(" world");

        assert!(input.undo());

        assert_eq!(input.text, "hello");
        assert_eq!(input.cursor, 5);
    }

    #[test]
    fn redo_brings_the_change_back() {
        let mut input = buffer("hello");
        input.checkpoint();
        input.insert_str(" world");
        input.undo();

        assert!(input.redo());

        assert_eq!(input.text, "hello world");
    }

    #[test]
    fn everything_since_the_last_checkpoint_undoes_together() {
        let mut input = buffer("");
        input.insert_str("one");
        input.insert_str(" two");

        assert!(input.undo());

        assert_eq!(input.text, "");
        assert!(!input.undo());
    }

    #[test]
    fn a_new_change_after_undo_drops_the_redo() {
        let mut input = buffer("a");
        input.checkpoint();
        input.insert_str("b");
        input.undo();
        input.insert_str("c");

        assert!(!input.redo());
        assert_eq!(input.text, "ac");
    }

    #[test]
    fn the_caret_goes_back_to_where_it_was_before_the_change() {
        let mut input = buffer("one two three");
        input.cursor = 4;
        input.checkpoint();
        input.text.replace_range(4..8, "");

        input.undo();
        assert_eq!(input.cursor, 4);

        input.cursor = 0;
        input.redo();
        assert_eq!(input.cursor, 4);
    }

    #[test]
    fn undo_brings_back_a_deleted_image() {
        let mut input = InputBuffer::new();
        input.images.push(PendingImage {
            data: vec![7],
            mime_type: mush_ai::types::ImageMimeType::Png,
            dimensions: None,
        });
        input.insert_char(super::super::IMAGE_PLACEHOLDER);
        input.checkpoint();
        input.backspace();
        assert!(input.images.is_empty());

        input.undo();

        assert_eq!(input.images.len(), 1);
        assert_eq!(input.images[0].data, vec![7]);
    }

    #[test]
    fn sending_the_draft_forgets_its_history() {
        let mut input = buffer("draft");
        input.checkpoint();

        input.take_text();

        assert!(!input.undo());
        assert_eq!(input.text, "");
    }
}
