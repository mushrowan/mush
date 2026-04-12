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
use crate::isolation::PaneIsolation;

pub use mush_ai::types::PaneId;

/// tracks a pane spawned by the delegate_task tool
#[derive(Debug, Clone)]
pub struct DelegationInfo {
    /// pane that requested this delegation
    pub from_pane: PaneId,
    /// task identifier for result routing
    pub task_id: String,
}

/// max agent turns for delegation sub-agents (prevents runaway resource usage)
pub const DELEGATION_MAX_TURNS: usize = 10;

/// per-pane state: independent agent session with display + conversation
pub struct Pane {
    pub id: PaneId,
    pub app: App,
    pub conversation: ConversationState,
    pub image_protos: HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    pub pending_prompt: Option<String>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
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
    /// set when this pane was spawned by delegate_task
    pub delegation: Option<DelegationInfo>,
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
            inbox: None,
            label: None,
            title_generated: false,
            area: Rect::default(),
            tools: None,
            cwd_override: None,
            isolation: None,
            delegation: None,
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
            inbox: None,
            label: None,
            title_generated: false,
            area: Rect::default(),
            tools: None,
            cwd_override: None,
            isolation: None,
            delegation: None,
        }
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
}

/// min column width before falling back to tabs
pub const MIN_COLUMN_WIDTH: u16 = 60;

/// how panes are arranged on screen
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// side-by-side vertical columns
    Columns,
    /// stacked with tab bar, only focused pane visible
    Tabs,
}

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

    // -- accessors --

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

    // -- focus management --

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

    // -- lifecycle --

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

    // -- layout --

    /// determine layout mode for a given terminal width
    pub fn layout_mode(&self, terminal_width: u16) -> LayoutMode {
        let n = self.panes.len() as u16;
        if n <= 1 {
            return LayoutMode::Columns;
        }
        // reserve 1 char per separator between columns
        let separators = n - 1;
        let usable = terminal_width.saturating_sub(separators);
        if usable / n >= MIN_COLUMN_WIDTH {
            LayoutMode::Columns
        } else {
            LayoutMode::Tabs
        }
    }

    /// compute layout rects for all panes, returns the active mode
    pub fn compute_layout(&mut self, area: Rect) -> LayoutMode {
        let mode = self.layout_mode(area.width);
        match mode {
            LayoutMode::Columns => {
                let n = self.panes.len() as u16;
                if n == 0 {
                    return mode;
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
            LayoutMode::Tabs => {
                let tab_h = if self.panes.len() > 1 { 1 } else { 0 };
                let content = Rect::new(
                    area.x,
                    area.y + tab_h,
                    area.width,
                    area.height.saturating_sub(tab_h),
                );
                for pane in &mut self.panes {
                    pane.area = content;
                }
            }
        }
        mode
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
    fn layout_columns_single_pane() {
        let mgr = PaneManager::new(test_pane(1));
        assert_eq!(mgr.layout_mode(80), LayoutMode::Columns);
    }

    #[test]
    fn layout_columns_two_panes_wide() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // (121 - 1 sep) / 2 = 60 >= MIN_COLUMN_WIDTH
        assert_eq!(mgr.layout_mode(121), LayoutMode::Columns);
    }

    #[test]
    fn layout_tabs_when_narrow() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.add_pane(test_pane(3));
        // (122 - 2 seps) / 3 = 40 < MIN_COLUMN_WIDTH
        assert_eq!(mgr.layout_mode(122), LayoutMode::Tabs);
    }

    #[test]
    fn layout_columns_exact_threshold() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // (121 - 1 sep) / 2 = 60 == MIN_COLUMN_WIDTH (passes >=)
        assert_eq!(mgr.layout_mode(121), LayoutMode::Columns);
        // (120 - 1 sep) / 2 = 59 < MIN_COLUMN_WIDTH
        assert_eq!(mgr.layout_mode(120), LayoutMode::Tabs);
    }

    #[test]
    fn compute_layout_columns_even() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // 121 wide: 1 separator, usable = 120, each col = 60
        let area = Rect::new(0, 0, 121, 40);
        let mode = mgr.compute_layout(area);
        assert_eq!(mode, LayoutMode::Columns);
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
    fn compute_layout_tabs() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.add_pane(test_pane(3));
        let area = Rect::new(0, 0, 100, 40);
        let mode = mgr.compute_layout(area);
        assert_eq!(mode, LayoutMode::Tabs);
        // all panes get same content area below 1-line tab bar
        for pane in mgr.panes() {
            assert_eq!(pane.area, Rect::new(0, 1, 100, 39));
        }
    }

    #[test]
    fn compute_layout_single_pane_full_area() {
        let mut mgr = PaneManager::new(test_pane(1));
        let area = Rect::new(0, 0, 80, 24);
        let mode = mgr.compute_layout(area);
        assert_eq!(mode, LayoutMode::Columns);
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
        let mode = mgr.compute_layout(area);
        assert_eq!(mode, LayoutMode::Columns);
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

    #[test]
    fn narrow_terminal_multi_pane_tabs() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // too narrow for columns, falls back to tabs
        let area = Rect::new(0, 0, 80, 24);
        let mode = mgr.compute_layout(area);
        assert_eq!(mode, LayoutMode::Tabs);
    }
}
