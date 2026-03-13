# terminal capabilities

mush adapts to the terminal it's running in. three capabilities are
configurable, each with auto-detection and manual overrides.

## keyboard enhancement

controls whether mush uses the kitty keyboard protocol for disambiguated
key events. this enables reliable detection of ctrl+shift+enter (pane fork),
ctrl+tab (pane switch), and other modified keys that are ambiguous in
traditional terminals.

| mode | behaviour |
|------|-----------|
| `auto` (default) | query the terminal, enable if supported |
| `enabled` | always enable (may cause issues in unsupported terminals) |
| `disabled` | never enable, fall back to traditional key handling |

**supported terminals**: kitty, wezterm, foot, ghostty, rio, and others
implementing the kitty keyboard protocol.

## mouse tracking

controls whether mush captures mouse events for scroll wheel handling
in the message list.

| mode | behaviour |
|------|-----------|
| `minimal` (default) | capture scroll events only |
| `disabled` | no mouse capture (use page up/down for scrolling) |

mouse tracking in `minimal` mode only captures the scroll wheel, it doesn't
interfere with text selection in terminals that support it.

## image probe

controls whether mush probes the terminal for inline image support
(sixel, kitty graphics, iterm2 inline images, halfblocks fallback).

| mode | behaviour |
|------|-----------|
| `auto` (default) | probe the terminal at startup |
| `disabled` | skip probing, no inline images |

disabling this avoids the brief probe delay at startup and any visual
artefacts on terminals that respond incorrectly to image protocol queries.

## configuration

### config.toml

```toml
[terminal]
keyboard_enhancement = "auto"   # auto | enabled | disabled
mouse_tracking = "minimal"      # minimal | disabled
image_probe = "auto"            # auto | disabled
```

### environment variables

environment variables override config.toml values. useful for per-session
tweaks or CI environments.

| variable | values |
|----------|--------|
| `MUSH_TUI_KEYBOARD_ENHANCEMENT` | auto, enabled, disabled, on, off, true, false, 0, 1 |
| `MUSH_TUI_MOUSE_TRACKING` | minimal, enabled, disabled, on, off, true, false, 0, 1, none |
| `MUSH_TUI_IMAGE_PROBE` | auto, enabled, disabled, on, off, true, false, 0, 1, none |

the env var parsing is deliberately lenient: `on`/`true`/`1`/`enabled`/`enable`
all work, as do `off`/`false`/`0`/`disabled`/`disable`/`none`.

### CLI flags

```
mush --keyboard-enhancement disabled
mush --mouse-tracking disabled
mush --image-probe disabled
```

### precedence

CLI flags > environment variables > config.toml > defaults.

the override chain is implemented as `TerminalPolicy::with_overrides()`,
which takes a `TerminalPolicyOverrides` struct where each field is `Option`.
only `Some` values replace the base policy.

## troubleshooting

### broken key handling in tmux/screen

tmux and older screen versions don't pass through the kitty keyboard protocol.
if keys like ctrl+shift+enter don't work:

```toml
[terminal]
keyboard_enhancement = "disabled"
```

or set `MUSH_TUI_KEYBOARD_ENHANCEMENT=off` before running mush.

### scroll not working

if page up/down and mouse scroll don't work, check that mouse tracking
isn't disabled. the default `minimal` mode should work in all modern terminals.

### image display issues

if images cause visual glitches or the terminal hangs briefly at startup,
disable the image probe:

```toml
[terminal]
image_probe = "disabled"
```

images in tool results will be described as text instead of rendered inline.

## implementation

the terminal policy flows through the system as:

1. `Config` parses `[terminal]` section from config.toml
2. CLI flags produce `TerminalPolicyOverrides`
3. env vars checked in `main.rs`, merged into overrides
4. `TerminalPolicy::with_overrides()` produces the final policy
5. policy passed to `TuiConfig`, used during terminal setup

the actual terminal setup happens in `mush-tui/src/runner/terminal.rs`:
- `enter_tui_terminal()` applies the policy (enables alt screen, raw mode, optional keyboard enhancement, optional mouse tracking)
- `probe_image_picker()` tests image support if `image_probe` is not disabled
- `cleanup()` restores the terminal to its previous state
- `install_panic_cleanup_hook()` ensures cleanup runs on panics
