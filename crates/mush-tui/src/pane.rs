//! pane management for multi-agent split views
//!
//! each pane wraps an independent agent session with its own conversation,
//! display state, and session tree. the pane manager handles layout
//! (columns or tabs) and focus switching

use std::collections::HashMap;
use std::sync::Arc;

use mush_ai::types::Message;
use mush_session::tree::SessionTree;
use ratatui::layout::Rect;
use tokio::sync::Mutex;

use crate::app::App;

/// unique pane identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u32);

impl PaneId {
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// per-pane state: independent agent session with display + conversation
pub struct Pane {
    pub id: PaneId,
    pub app: App,
    pub conversation: Vec<Message>,
    pub session_tree: SessionTree,
    pub image_protos:
        HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    pub pending_prompt: Option<String>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    /// user-facing label (auto-generated or manual)
    pub label: Option<String>,
    /// layout rect computed each frame by PaneManager
    pub area: Rect,
}

impl Pane {
    /// create a new empty pane
    pub fn new(id: PaneId, app: App) -> Self {
        Self {
            id,
            app,
            conversation: Vec::new(),
            session_tree: SessionTree::new(),
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            label: None,
            area: Rect::default(),
        }
    }

    /// create with existing conversation (for session resume or forking)
    pub fn with_conversation(
        id: PaneId,
        app: App,
        conversation: Vec<Message>,
        session_tree: SessionTree,
    ) -> Self {
        Self {
            id,
            app,
            conversation,
            session_tree,
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            label: None,
            area: Rect::default(),
        }
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
}

impl PaneManager {
    /// create with a single initial pane
    pub fn new(initial: Pane) -> Self {
        let next_id = initial.id.0 + 1;
        Self {
            panes: vec![initial],
            focused: 0,
            next_id,
        }
    }

    /// allocate the next pane id
    pub fn next_id(&mut self) -> PaneId {
        let id = PaneId(self.next_id);
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
        if n <= 1 || terminal_width / n >= MIN_COLUMN_WIDTH {
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
                let col_width = area.width / n;
                let remainder = area.width % n;
                let mut x = area.x;
                for (i, pane) in self.panes.iter_mut().enumerate() {
                    let w = col_width + if (i as u16) < remainder { 1 } else { 0 };
                    pane.area = Rect::new(x, area.y, w, area.height);
                    x += w;
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

    /// pane indices with unread activity (not focused)
    pub fn panes_with_activity(&self) -> Vec<(usize, PaneId)> {
        self.panes
            .iter()
            .enumerate()
            .filter(|(i, p)| *i != self.focused && p.app.has_unread)
            .map(|(i, p)| (i, p.id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pane(id: u32) -> Pane {
        Pane::new(PaneId(id), App::new("test".into(), 200_000))
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

        let removed = mgr.remove_pane(PaneId(1));
        assert!(removed.is_some());
        assert_eq!(mgr.pane_count(), 2);
        assert_eq!(mgr.focused_index(), 1); // shifted left
    }

    #[test]
    fn remove_focused_pane() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.focus_index(1);

        let removed = mgr.remove_pane(PaneId(2));
        assert!(removed.is_some());
        assert_eq!(mgr.pane_count(), 1);
        assert_eq!(mgr.focused_index(), 0);
    }

    #[test]
    fn remove_last_pane_returns_none() {
        let mut mgr = PaneManager::new(test_pane(1));
        assert!(mgr.remove_pane(PaneId(1)).is_none());
        assert_eq!(mgr.pane_count(), 1);
    }

    #[test]
    fn remove_nonexistent_pane() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        assert!(mgr.remove_pane(PaneId(99)).is_none());
        assert_eq!(mgr.pane_count(), 2);
    }

    #[test]
    fn close_focused() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.focus_index(1);
        let closed = mgr.close_focused();
        assert!(closed.is_some());
        assert_eq!(closed.unwrap().id, PaneId(2));
        assert_eq!(mgr.focused_index(), 0);
    }

    #[test]
    fn next_id_increments() {
        let mut mgr = PaneManager::new(test_pane(1));
        assert_eq!(mgr.next_id(), PaneId(2));
        assert_eq!(mgr.next_id(), PaneId(3));
        assert_eq!(mgr.next_id(), PaneId(4));
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
        // 120 / 2 = 60 >= MIN_COLUMN_WIDTH
        assert_eq!(mgr.layout_mode(120), LayoutMode::Columns);
    }

    #[test]
    fn layout_tabs_when_narrow() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.add_pane(test_pane(3));
        // 120 / 3 = 40 < MIN_COLUMN_WIDTH
        assert_eq!(mgr.layout_mode(120), LayoutMode::Tabs);
    }

    #[test]
    fn layout_columns_exact_threshold() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        // 120 / 2 = 60 == MIN_COLUMN_WIDTH (passes >=)
        assert_eq!(mgr.layout_mode(120), LayoutMode::Columns);
        // 118 / 2 = 59 < MIN_COLUMN_WIDTH
        assert_eq!(mgr.layout_mode(118), LayoutMode::Tabs);
    }

    #[test]
    fn compute_layout_columns_even() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        let area = Rect::new(0, 0, 120, 40);
        let mode = mgr.compute_layout(area);
        assert_eq!(mode, LayoutMode::Columns);
        assert_eq!(mgr.panes()[0].area, Rect::new(0, 0, 60, 40));
        assert_eq!(mgr.panes()[1].area, Rect::new(60, 0, 60, 40));
    }

    #[test]
    fn compute_layout_columns_odd_width() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        let area = Rect::new(0, 0, 121, 40);
        mgr.compute_layout(area);
        // 121 / 2 = 60 remainder 1, first column gets extra pixel
        assert_eq!(mgr.panes()[0].area, Rect::new(0, 0, 61, 40));
        assert_eq!(mgr.panes()[1].area, Rect::new(61, 0, 60, 40));
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
        assert!(mgr.pane(PaneId(1)).is_some());
        assert!(mgr.pane(PaneId(2)).is_some());
        assert!(mgr.pane(PaneId(99)).is_none());
    }

    #[test]
    fn three_column_layout() {
        let mut mgr = PaneManager::new(test_pane(1));
        mgr.add_pane(test_pane(2));
        mgr.add_pane(test_pane(3));
        let area = Rect::new(0, 0, 180, 40);
        let mode = mgr.compute_layout(area);
        assert_eq!(mode, LayoutMode::Columns);
        assert_eq!(mgr.panes()[0].area, Rect::new(0, 0, 60, 40));
        assert_eq!(mgr.panes()[1].area, Rect::new(60, 0, 60, 40));
        assert_eq!(mgr.panes()[2].area, Rect::new(120, 0, 60, 40));
    }
}
