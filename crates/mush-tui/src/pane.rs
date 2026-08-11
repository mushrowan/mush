//! pane management for multi-agent split views
//!
//! each pane wraps an independent agent session with its own conversation,
//! display state, and session tree. the pane manager handles layout
//! (columns or tabs) and focus switching

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use mush_agent::tool::ToolRegistry;
use mush_ai::types::Message;
use mush_session::ConversationState;
use ratatui::layout::Rect;
use tokio::sync::Mutex;

use crate::app::App;
use crate::input_buffer::PendingImage;
use crate::isolation::PaneIsolation;

pub use mush_ai::types::PaneId;

#[derive(Debug, Clone)]
pub struct PendingPrompt {
    pub text: String,
    pub images: Vec<PendingImage>,
    persisted: bool,
}

impl PendingPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            persisted: false,
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<PendingImage>) -> Self {
        Self {
            text: text.into(),
            images,
            persisted: false,
        }
    }

    pub fn is_persisted(&self) -> bool {
        self.persisted
    }

    pub fn mark_persisted(&mut self) {
        self.persisted = true;
    }
}

/// per-pane state: independent agent session with display + conversation
pub struct Pane {
    pub id: PaneId,
    pub app: App,
    pub conversation: ConversationState,
    pub image_protos: HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    pub pending_prompt: Option<PendingPrompt>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    /// inputs the user submitted while a /compact (or /fork-compact)
    /// task was running. each entry is `(text, image_bytes)` mirroring
    /// what UserSubmit would normally have done. drained by the
    /// runtime once compaction lands so they replay as a fresh turn
    /// instead of getting silently dropped or clobbering the input box
    pub pending_compaction_inputs: Vec<(String, Vec<PendingImage>)>,
    /// inbox for messages from sibling panes
    pub inbox: Option<tokio::sync::mpsc::UnboundedReceiver<crate::messaging::InterPaneMessage>>,
    /// user-facing label (auto-generated or manual)
    pub label: Option<String>,
    /// whether an LLM title has already been generated
    pub title_generated: bool,
    /// layout rect computed each frame by PaneManager
    pub area: Rect,
    /// per-pane tools when cwd differs (worktree isolation)
    pub tools: Option<ToolRegistry>,
    /// per-pane cwd override (worktree isolation)
    pub cwd_override: Option<PathBuf>,
    /// VCS isolation state
    pub isolation: Option<PaneIsolation>,
}

impl Pane {
    /// create a new empty pane
    pub fn new(id: PaneId, app: App) -> Self {
        Self {
            id,
            app,
            conversation: ConversationState::new(),
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            pending_compaction_inputs: Vec::new(),
            inbox: None,
            label: None,
            title_generated: false,
            area: Rect::default(),
            tools: None,
            cwd_override: None,
            isolation: None,
        }
    }

    /// create with existing conversation (for session resume or forking)
    pub fn with_conversation(id: PaneId, app: App, conversation: ConversationState) -> Self {
        Self {
            id,
            app,
            conversation,
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            pending_compaction_inputs: Vec::new(),
            inbox: None,
            label: None,
            title_generated: false,
            area: Rect::default(),
            tools: None,
            cwd_override: None,
            isolation: None,
        }
    }

    /// stash a user submit while a /compact run is in flight.
    /// records the (text, images) for later replay, surfaces a
    /// queued (dimmed) bubble in the chat so the user sees it didn't
    /// vanish, and clears the input buffer so they can keep typing
    pub fn queue_input_during_compaction(&mut self, text: String, images: Vec<PendingImage>) {
        let image_bytes: Vec<Vec<u8>> = images.iter().map(|image| image.data.clone()).collect();
        if image_bytes.is_empty() {
            self.app.push_queued_message(text.clone());
        } else {
            self.app
                .push_queued_message_with_images(text.clone(), image_bytes);
        }
        self.pending_compaction_inputs.push((text, images));
        self.app.input.text.clear();
        self.app.input.cursor = 0;
    }

    /// drain everything queued during the just-finished compaction.
    /// also clears the queued bubbles from the display so the runtime
    /// can render them as real (non-queued) user messages without
    /// duplicates
    pub fn drain_compaction_inputs(&mut self) -> Vec<(String, Vec<PendingImage>)> {
        let drained = std::mem::take(&mut self.pending_compaction_inputs);
        let _ = self.app.take_queued_messages();
        drained
    }

    /// mutable references to all major fields at once,
    /// enabling the borrow checker to see disjoint borrows
    pub fn fields_mut(
        &mut self,
    ) -> (
        &mut App,
        &mut ConversationState,
        &mut HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    ) {
        (
            &mut self.app,
            &mut self.conversation,
            &mut self.image_protos,
        )
    }

    /// ensure `image_protos` has a protocol for every `(msg_idx, tc_idx)`
    /// referenced by `render_areas`, creating missing entries on demand
    /// from the raw bytes stored on the matching message or tool call.
    ///
    /// this is a safety net for paths that register image render areas
    /// without eagerly creating a protocol: session reloads (protocols
    /// aren't persisted), steering submits with attached images, and
    /// any future pushes via `push_user_message_with_images` from a
    /// code path that forgets to poke `image_protos`
    pub fn ensure_image_protocols(
        &mut self,
        picker: &ratatui_image::picker::Picker,
        render_areas: &[crate::display_types::ImageRenderArea],
    ) {
        use crate::display_types::MessageRole;
        for area in render_areas {
            let key = (area.msg_idx, area.tc_idx);
            if self.image_protos.contains_key(&key) {
                continue;
            }
            let Some(msg) = self.app.messages.get(area.msg_idx) else {
                continue;
            };
            let bytes: Option<&[u8]> = if msg.role == MessageRole::User {
                msg.images.get(area.tc_idx).map(Vec::as_slice)
            } else {
                msg.tool_calls
                    .get(area.tc_idx)
                    .and_then(|tc| tc.image_data.as_deref())
            };
            let Some(bytes) = bytes else { continue };
            let Ok(dyn_img) = image::load_from_memory(bytes) else {
                continue;
            };
            self.image_protos
                .insert(key, picker.new_resize_protocol(dyn_img));
        }
    }
}

/// narrowest a pane column gets
pub const MIN_COLUMN_WIDTH: u16 = 60;

/// manages multiple panes: layout, focus, lifecycle
pub struct PaneManager {
    panes: Vec<Pane>,
    focused: usize,
    next_id: u32,
    /// per-pane width offset in columns mode (positive = wider, negative = narrower)
    width_offsets: HashMap<PaneId, i16>,
}

impl PaneManager {
    /// create with a single initial pane
    pub fn new(initial: Pane) -> Self {
        let next_id = initial.id.as_u32() + 1;
        Self {
            panes: vec![initial],
            focused: 0,
            next_id,
            width_offsets: HashMap::new(),
        }
    }

    /// allocate the next pane id
    pub fn next_id(&mut self) -> PaneId {
        let id = PaneId::new(self.next_id);
        self.next_id += 1;
        id
    }

    // accessors
    pub fn focused(&self) -> &Pane {
        &self.panes[self.focused]
    }

    pub fn focused_mut(&mut self) -> &mut Pane {
        &mut self.panes[self.focused]
    }

    pub fn focused_index(&self) -> usize {
        self.focused
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn panes(&self) -> &[Pane] {
        &self.panes
    }

    pub fn panes_mut(&mut self) -> &mut [Pane] {
        &mut self.panes
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    pub fn is_multi_pane(&self) -> bool {
        self.panes.len() > 1
    }

    // focus management
    pub fn focus_next(&mut self) {
        if self.panes.len() > 1 {
            self.focused = (self.focused + 1) % self.panes.len();
        }
    }

    pub fn focus_prev(&mut self) {
        if self.panes.len() > 1 {
            self.focused = if self.focused == 0 {
                self.panes.len() - 1
            } else {
                self.focused - 1
            };
        }
    }

    pub fn focus_index(&mut self, idx: usize) {
        if idx < self.panes.len() {
            self.focused = idx;
        }
    }

    // lifecycle
    /// add a pane, returns its index
    pub fn add_pane(&mut self, pane: Pane) -> usize {
        let idx = self.panes.len();
        self.panes.push(pane);
        idx
    }

    /// remove a pane by id. returns None if it's the last pane
    pub fn remove_pane(&mut self, id: PaneId) -> Option<Pane> {
        if self.panes.len() <= 1 {
            return None;
        }
        let idx = self.panes.iter().position(|p| p.id == id)?;
        let pane = self.panes.remove(idx);
        if self.focused >= self.panes.len() {
            self.focused = self.panes.len() - 1;
        } else if idx < self.focused {
            self.focused -= 1;
        }
        Some(pane)
    }

    /// close the focused pane. returns None if it's the last pane
    pub fn close_focused(&mut self) -> Option<Pane> {
        let id = self.focused().id;
        self.remove_pane(id)
    }

    // layout
    /// lay the panes out as side-by-side columns
    pub fn compute_layout(&mut self, area: Rect) {
        {
            {
                let n = self.panes.len() as u16;
                if n == 0 {
                    return;
                }
                // single pane: no separators
                let separators = n.saturating_sub(1);
                let usable = area.width.saturating_sub(separators);
                let base_width = usable / n;
                let remainder = usable % n;

                // compute widths with offsets applied
                let mut widths: Vec<u16> = self
                    .panes
                    .iter()
                    .enumerate()
                    .map(|(i, pane)| {
                        let base = base_width + if (i as u16) < remainder { 1 } else { 0 };
                        let offset = self.width_offsets.get(&pane.id).copied().unwrap_or(0);
                        let min_w = (MIN_COLUMN_WIDTH as i32).min(usable as i32);
                        (base as i32 + offset as i32).clamp(min_w, usable as i32) as u16
                    })
                    .collect();

                // normalise: ensure total width matches usable space
                let total: u16 = widths.iter().sum();
                if total != usable && !widths.is_empty() {
                    let diff = usable as i32 - total as i32;
                    // distribute diff across the last pane (allow down to 20 cols)
                    let last = widths.len() - 1;
                    widths[last] = (widths[last] as i32 + diff).max(20) as u16;
                }

                let mut x = area.x;
                for (i, pane) in self.panes.iter_mut().enumerate() {
                    pane.area = Rect::new(x, area.y, widths[i], area.height);
                    x += widths[i];
                    // leave 1-char gap for separator (except after last)
                    if (i as u16) < n - 1 {
                        x += 1;
                    }
                }
            }
        }
    }

    /// resize the focused pane by delta columns (negative = shrink)
    pub fn resize_focused(&mut self, delta: i16) {
        if self.panes.len() <= 1 {
            return;
        }
        let id = self.panes[self.focused].id;
        let offset = self.width_offsets.entry(id).or_insert(0);
        *offset += delta;
    }

    /// pane indices with unread activity (not focused)
    pub fn panes_with_activity(&self) -> Vec<(usize, PaneId)> {
        self.panes
            .iter()
            .enumerate()
            .filter(|(i, p)| *i != self.focused && p.app.navigation.has_unread)
            .map(|(i, p)| (i, p.id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;

    fn test_pane(id: u32) -> Pane {
        Pane::new(
            PaneId::new(id),
            App::new("test".into(), TokenCount::new(200_000)),
        )
    }

    fn pending_png(data: Vec<u8>) -> PendingImage {
        PendingImage {
            data,
            mime_type: mush_ai::types::ImageMimeType::Png,
            dimensions: None,
        }
    }

    #[test]
    fn single_pane_basics() {
        let mgr = PaneManager::new(test_pane(1));
        assert_eq!(mgr.pane_count(), 1);
        assert_eq!(mgr.focused_index(), 0);
        assert!(!mgr.is_multi_pane());
    }

    #[test]
    fn add_and_focus_cycle() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.add_pane(test_pane(3));
        assert_eq!(mgr.pane_count(), 3);
        assert!(mgr.is_multi_pane());

        mgr.focus_next();
        assert_eq!(mgr.focused_index(), 1);
        mgr.focus_next();
        assert_eq!(mgr.focused_index(), 2);
        mgr.focus_next();
        assert_eq!(mgr.focused_index(), 0); // wraps

        mgr.focus_prev();
        assert_eq!(mgr.focused_index(), 2); // wraps back
    }

    #[test]
    fn focus_by_index() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.focus_index(1);
        assert_eq!(mgr.focused_index(), 1);
        mgr.focus_index(99); // out of bounds, no change
        assert_eq!(mgr.focused_index(), 1);
    }

    #[test]
    fn focus_single_pane_is_noop() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.focus_next();
        assert_eq!(mgr.focused_index(), 0);
        mgr.focus_prev();
        assert_eq!(mgr.focused_index(), 0);
    }

    #[test]
    fn remove_pane_before_focused() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.add_pane(test_pane(3));
        mgr.focus_index(2);

        let removed = mgr.remove_pane(PaneId::new(1));
        assert!(removed.is_some());
        assert_eq!(mgr.pane_count(), 2);
        assert_eq!(mgr.focused_index(), 1); // shifted left
    }

    #[test]
    fn remove_focused_pane() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.focus_index(1);

        let removed = mgr.remove_pane(PaneId::new(2));
        assert!(removed.is_some());
        assert_eq!(mgr.pane_count(), 1);
        assert_eq!(mgr.focused_index(), 0);
    }

    #[test]
    fn remove_last_pane_returns_none() {
        let mut mgr = PaneManager::new(test_pane(1));
        assert!(mgr.remove_pane(PaneId::new(1)).is_none());
        assert_eq!(mgr.pane_count(), 1);
    }

    #[test]
    fn remove_nonexistent_pane() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        assert!(mgr.remove_pane(PaneId::new(99)).is_none());
        assert_eq!(mgr.pane_count(), 2);
    }

    #[test]
    fn close_focused() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.focus_index(1);
        let closed = mgr.close_focused();
        assert!(closed.is_some());
        assert_eq!(closed.unwrap().id, PaneId::new(2));
        assert_eq!(mgr.focused_index(), 0);
    }

    #[test]
    fn next_id_increments() {
        let mut mgr = PaneManager::new(test_pane(1));
        assert_eq!(mgr.next_id(), PaneId::new(2));
        assert_eq!(mgr.next_id(), PaneId::new(3));
        assert_eq!(mgr.next_id(), PaneId::new(4));
    }

    #[test]
    fn compute_layout_columns_even() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // 121 wide: 1 separator, usable = 120, each col = 60
        let area = Rect::new(0, 0, 121, 40);
        mgr.compute_layout(area);
        assert_eq!(mgr.panes()[0].area, Rect::new(0, 0, 60, 40));
        // x=60 content + 1 sep = 61
        assert_eq!(mgr.panes()[1].area, Rect::new(61, 0, 60, 40));
    }

    #[test]
    fn compute_layout_columns_odd_width() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // 122 wide: 1 sep, usable = 121, col = 60 remainder 1
        let area = Rect::new(0, 0, 122, 40);
        mgr.compute_layout(area);
        assert_eq!(mgr.panes()[0].area, Rect::new(0, 0, 61, 40));
        // x=61 content + 1 sep = 62
        assert_eq!(mgr.panes()[1].area, Rect::new(62, 0, 60, 40));
    }

    #[test]
    fn compute_layout_single_pane_full_area() {
        let mut mgr = PaneManager::new(test_pane(1));
        let area = Rect::new(0, 0, 80, 24);
        mgr.compute_layout(area);
        assert_eq!(mgr.panes()[0].area, Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn pane_lookup_by_id() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        assert!(mgr.pane(PaneId::new(1)).is_some());
        assert!(mgr.pane(PaneId::new(2)).is_some());
        assert!(mgr.pane(PaneId::new(99)).is_none());
    }

    #[test]
    fn three_column_layout() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.add_pane(test_pane(3));
        // 182 wide: 2 separators, usable = 180, each col = 60
        let area = Rect::new(0, 0, 182, 40);
        mgr.compute_layout(area);
        assert_eq!(mgr.panes()[0].area, Rect::new(0, 0, 60, 40));
        assert_eq!(mgr.panes()[1].area, Rect::new(61, 0, 60, 40));
        assert_eq!(mgr.panes()[2].area, Rect::new(122, 0, 60, 40));
    }

    #[test]
    fn resize_focused_pane() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // 121 wide: 1 sep, usable = 120, base = 60 each
        let area = Rect::new(0, 0, 121, 40);

        // grow pane 1 by 10 cols
        mgr.resize_focused(10);
        mgr.compute_layout(area);
        assert_eq!(mgr.panes()[0].area.width, 70);
        // pane 2 shrinks to compensate
        assert_eq!(mgr.panes()[1].area.width, 50);
    }

    #[test]
    fn resize_single_pane_is_noop() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.resize_focused(10);
        let area = Rect::new(0, 0, 80, 24);
        mgr.compute_layout(area);
        assert_eq!(mgr.panes()[0].area.width, 80);
    }

    #[test]
    fn resize_clamps_to_min_width() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // 121 wide: 1 sep, usable = 120, base = 60 each
        let area = Rect::new(0, 0, 121, 40);

        // try to shrink pane 1 below min width
        mgr.resize_focused(-100);
        mgr.compute_layout(area);
        // clamped to MIN_COLUMN_WIDTH
        assert_eq!(mgr.panes()[0].area.width, MIN_COLUMN_WIDTH);
    }

    #[test]
    fn narrow_terminal_single_pane_no_panic() {
        let mut mgr = PaneManager::new(test_pane(1));
        // terminal narrower than MIN_COLUMN_WIDTH - should not panic
        let area = Rect::new(0, 0, 46, 24);
        mgr.compute_layout(area);
        assert_eq!(mgr.panes()[0].area.width, 46);
    }

    /// build a minimal valid png (1x1 red pixel) for protocol-ensure tests
    fn tiny_png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn ensure_image_protocols_creates_proto_for_user_message_image() {
        // regression: push_user_message_with_images (fresh submit, steering
        // submit, session reload rehydration) can leave image_protos empty
        // for a message that has real bytes in msg.images. the render path's
        // safety net must lazily create a StatefulProtocol from those bytes
        // so the attachment box isn't left blank
        let mut pane = test_pane(1);
        pane.app
            .push_user_message_with_images("look here", vec![tiny_png_bytes()]);
        assert!(
            pane.image_protos.is_empty(),
            "precondition: image_protos starts empty"
        );
        let picker = ratatui_image::picker::Picker::halfblocks();
        let render_areas = vec![crate::display_types::ImageRenderArea {
            msg_idx: 0,
            tc_idx: 0,
            area: Rect::new(0, 0, 10, 5),
        }];
        pane.ensure_image_protocols(&picker, &render_areas);
        assert!(
            pane.image_protos.contains_key(&(0, 0)),
            "protocol should have been lazy-created from msg.images[0] bytes"
        );
    }

    #[test]
    fn ensure_image_protocols_is_idempotent() {
        // repeated calls (every frame) must not thrash: if the key is
        // already present we skip the image decode + protocol create
        let mut pane = test_pane(1);
        pane.app
            .push_user_message_with_images("look here", vec![tiny_png_bytes()]);
        let picker = ratatui_image::picker::Picker::halfblocks();
        let render_areas = vec![crate::display_types::ImageRenderArea {
            msg_idx: 0,
            tc_idx: 0,
            area: Rect::new(0, 0, 10, 5),
        }];
        pane.ensure_image_protocols(&picker, &render_areas);
        let len_after_first = pane.image_protos.len();
        pane.ensure_image_protocols(&picker, &render_areas);
        assert_eq!(
            pane.image_protos.len(),
            len_after_first,
            "repeated ensure should be a no-op when proto already exists"
        );
    }

    #[test]
    fn ensure_image_protocols_tolerates_missing_bytes() {
        // render_areas may reference an index with no matching image bytes
        // (e.g. a stale area from a previous frame). must not panic or
        // create a bogus entry
        let mut pane = test_pane(1);
        pane.app.push_user_message("no images here");
        let picker = ratatui_image::picker::Picker::halfblocks();
        let render_areas = vec![crate::display_types::ImageRenderArea {
            msg_idx: 0,
            tc_idx: 0,
            area: Rect::new(0, 0, 10, 5),
        }];
        pane.ensure_image_protocols(&picker, &render_areas);
        assert!(pane.image_protos.is_empty(), "no bytes -> no proto created");
    }

    #[test]
    fn queue_input_during_compaction_records_text_and_shows_queued_bubble() {
        // text submitted while a /compact is running should appear as a
        // queued (dimmed) bubble in the chat AND be remembered in the
        // backlog so the runtime can replay it once compaction lands.
        // the input box must be cleared so the user can start typing
        // their next thought without manually erasing
        let mut pane = test_pane(1);
        pane.app.input.text = "hello".into();
        pane.app.input.cursor = 5;
        pane.queue_input_during_compaction("hello".into(), vec![]);

        assert_eq!(pane.pending_compaction_inputs.len(), 1);
        assert_eq!(pane.pending_compaction_inputs[0].0, "hello");
        assert!(pane.app.has_queued_messages());
        assert!(
            pane.app.input.text.is_empty(),
            "input buffer should clear after queuing"
        );
        assert_eq!(pane.app.input.cursor, 0);
    }

    #[test]
    fn drain_compaction_inputs_returns_entries_and_clears_state() {
        // post-compaction the runtime needs every queued submit (text +
        // images) to fire a fresh turn from. drain must hand them back
        // and remove the queued bubbles from the display so they
        // aren't shown twice
        let mut pane = test_pane(1);
        pane.queue_input_during_compaction("first".into(), vec![]);
        pane.queue_input_during_compaction("second".into(), vec![pending_png(vec![0u8, 1, 2])]);
        assert!(pane.app.has_queued_messages());

        let drained = pane.drain_compaction_inputs();

        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].0, "first");
        assert_eq!(drained[1].0, "second");
        assert_eq!(drained[1].1[0].data, vec![0u8, 1, 2]);
        assert!(pane.pending_compaction_inputs.is_empty());
        assert!(
            !pane.app.has_queued_messages(),
            "queued bubbles should be cleared after drain"
        );
    }
}
