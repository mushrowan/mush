//! app event and mode enums

/// events that flow between the TUI and the agent
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// user submitted a prompt
    UserSubmit {
        text: String,
    },
    /// user executed a slash command
    SlashCommand {
        action: crate::slash::SlashAction,
    },
    /// user toggled the currently selected item in the /settings overlay
    SettingsToggleSelected,
    /// user requested quit
    Quit,
    /// user requested abort of current operation
    Abort,
    /// user scrolled up/down
    ScrollUp(u16),
    ScrollDown(u16),
    /// resize
    Resize(u16, u16),
    /// user cycled thinking level
    CycleThinkingLevel,
    /// user triggered clipboard image paste
    PasteImage,
    /// split current pane (fork conversation into new agent)
    SplitPane,
    /// close the focused pane
    ClosePane,
    /// focus the next pane
    FocusNextPane,
    /// focus the previous pane
    FocusPrevPane,
    /// focus pane by index (0-based)
    FocusPaneByIndex(usize),
    /// resize focused pane (positive = grow, negative = shrink)
    ResizePane(i16),
    /// alt+k: edit a queued steering message
    EditSteering,
}

/// which UI mode the app is in
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    SessionPicker,
    /// slash command completion menu visible above input
    SlashComplete,
    /// waiting for user to confirm a tool call (y/n)
    ToolConfirm,
    /// scroll mode: j/k scroll, y copies message, esc exits
    Scroll,
    /// search mode: type to filter messages, enter to jump
    Search,
    /// settings overlay: j/k navigate, space/enter toggle, esc close
    Settings,
}
