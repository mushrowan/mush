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
    /// user picked a model from the model picker, switch to it
    ModelSelected {
        model_id: String,
    },
    /// user toggled a favourite via ctrl+f in the model picker; runner
    /// should persist the current `app.completion.favourite_models` list
    PersistFavourites,
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
    /// centred floating model picker (`/model` with no arg)
    ModelPicker,
    /// centred floating login picker (`/login` with no arg). lists each
    /// oauth provider with logged-in/out badges, enter starts a login
    /// flow or arms a logout confirm
    LoginPicker,
    /// slash command completion menu visible above input
    SlashComplete,
    /// `@<word>` template picker open after a tab on a partial-match
    /// trigger. tab cycles, enter inserts, esc closes without touching
    /// the input
    AtPicker,
    /// interactive slot editor for `$1`/`$2`/`$@` placeholders. opens
    /// after a template that contains placeholders is expanded; tab
    /// cycles between slots, enter / esc exit the mode and keep the
    /// typed text in place
    SlotEdit,
    /// waiting for user to confirm a tool call (y/n)
    ToolConfirm,
    /// scroll mode: j/k scroll, y copies message, esc exits
    Scroll,
    /// search mode: type to filter messages, enter to jump
    Search,
    /// settings overlay: j/k navigate, space/enter toggle, esc close
    Settings,
}
