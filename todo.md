# todo

## open items
- [ ] mush-ext: dynamic tool registration from extensions
- [ ] mush-ext: provider registration from extensions

## future ideas
- [ ] neovim plugin
- [ ] vscode extension

---

## multi-agent split panes

user-facing interaction: press ctrl+shift+enter to fork the current conversation
into a new agent pane. the window splits (like tmux) and a second agent starts
from the same conversation history. pressing ctrl+shift+enter again in any pane
creates yet another split, so you can have N agents running in parallel

### research summary (feb/mar 2026 landscape)

the multi-agent coding pattern exploded in early 2026. key references:

**claude code agent teams** (feb 2026, anthropic)
- team lead spawns 2-16 independent teammates, each with own context window
- communication via mailbox system (JSON files on disk, one per agent)
- shared task list for coordination (`~/.claude/tasks/{team-name}/`)
- peer-to-peer messaging via `SendMessage` tool
- tmux/iterm2 split panes for visual per-agent monitoring
- 3-7x token cost vs single session, offset by parallelism gains

**opencode agent teams** (feb 2026, sst/opencode)
- same concept ported to go, with key architectural differences
- in-process agents (no separate OS processes), JSONL inbox files
- event-driven message delivery via session injection + auto-wake
  (no polling, unlike claude code's file-polling approach)
- two state machines per agent: coarse lifecycle + fine execution status
- sub-agents explicitly denied team messaging tools (prevents flood)
- supports mixing models from different providers in the same team

**adaptorch** (feb 2026, arxiv 2602.16873)
- formal framework for task-adaptive multi-agent orchestration
- dynamically selects among parallel/sequential/hierarchical/hybrid topologies
- key insight: orchestration topology matters more than individual model capability
  once models reach comparable benchmark performance

**google's eight patterns** (jan 2026)
- supervisor, hierarchical, sequential, parallel, swarm, reflection, tool-use,
  state-shared
- state-shared is most critical: centralised context store all agents read/write
- swarm pattern: agents subscribe to codebase changes, auto-trigger updates

**core patterns across all implementations:**
1. each agent gets its own context window (independent LLM sessions)
2. agents share a message prefix (conversation history up to the fork point)
3. inter-agent communication via message passing (not shared mutable state)
4. file/directory ownership to prevent write conflicts
5. a coordination mechanism (task list, mailbox, or shared state file)

### design for mush

**interaction model:**
- ctrl+shift+enter on a prompt forks the conversation
- the prompt is sent to the new agent, not the current one
- the current pane keeps its state (can keep working independently)
- ctrl+shift+enter again in any pane creates another fork
- each pane is a fully independent agent loop with its own streaming
- all forked agents share the conversation prefix up to the fork point
  (maximises prompt cache hits on providers that support it)

**TUI layout:**
- single pane by default (current behaviour)
- first split: vertical 50/50 (left = original, right = new agent)
- subsequent splits: subdivide the focused pane
- each pane has its own: message list, input box, status bar
- focused pane highlighted (border colour or indicator)
- ctrl+arrow or alt+arrow to switch focus between panes
- a pane can be closed with /close or ctrl+w, remaining panes reflow
- status bar shows pane id/label for orientation

**agent architecture:**
- each pane runs its own `agent_loop()` stream independently
- the `SessionTree` naturally supports this: branch from the fork point,
  each pane walks its own leaf→root path for context
- shared `ApiRegistry` and tools (already `Arc`-wrapped)
- separate `App` state per pane (messages, scroll, streaming buffers)
- single tokio runtime, multiple agent streams multiplexed via `select!`

**inter-agent messaging:**
- lightweight mailbox: `tokio::sync::broadcast` or `mpsc` channels
- a `send_message` tool available to each agent (knows about sibling panes)
- messages from siblings appear as system messages in the recipient's context
- auto-wake: if a pane's agent is idle when a message arrives, optionally
  restart its loop with the new message as input
- agents told about siblings in their system prompt ("you are agent 2 of 3,
  working alongside agent 1 (doing X) and agent 3 (doing Y)")

**session persistence:**
- forked conversations are branches in the same SessionTree
- each pane's branch saved independently
- on resume, only the "main" branch loads by default
- /tree shows all branches including forked agent work

**cost tracking:**
- per-pane token/cost counters in each pane's status bar
- /cost shows aggregate across all panes
- shared cache hits benefit all panes (same prefix)

### implementation phases

#### phase A: pane infrastructure (TUI)
- [ ] `Pane` struct: wraps `App` state + pane id + layout rect
- [ ] `PaneManager`: owns Vec<Pane>, tracks focused pane, handles layout
- [ ] layout algorithm: recursive binary splits (like tmux)
- [ ] render loop draws each pane independently into its allocated rect
- [ ] focus switching: ctrl+arrow keys to navigate between panes
- [ ] pane border styling (focused vs unfocused)
- [ ] /close and ctrl+w to close a pane, reflow remaining
- [ ] single-pane mode is just PaneManager with one pane (no regression)

#### phase B: forking agent sessions
- [ ] ctrl+shift+enter handler: creates new pane + branches SessionTree
- [ ] new pane inherits conversation history up to fork point
- [ ] new pane gets its own agent_loop() stream
- [ ] multiplexing: select! over all active pane streams + terminal events
- [ ] the prompt typed at fork time goes to the new pane's agent
- [ ] each pane has independent model, thinking level, streaming state
- [ ] pane-local /model, /compact, /undo work independently

#### phase C: inter-agent communication
- [ ] message bus: broadcast channel connecting all active panes
- [ ] `send_message` tool: agent can send text to a specific sibling by id
- [ ] received messages injected as system messages into recipient context
- [ ] auto-wake idle agents when they receive a message
- [ ] system prompt additions: sibling awareness ("you are pane 2 of 3")
- [ ] /broadcast slash command: user sends a message to all panes

#### phase D: polish and UX
- [ ] pane labels (auto-generated from first prompt or user-assigned)
- [ ] aggregate /cost across all panes
- [ ] pane-specific status bar with pane id and sibling count
- [ ] ctrl+shift+arrow to resize panes
- [ ] /panes command to list all active panes and their status
- [ ] session save/resume with multi-pane state
- [ ] print mode support: -p with --panes flag for parallel agents to stdout

### key decisions to make during implementation
- split direction: always vertical? or alternate h/v like a tiling WM?
- max panes: hard limit (4? 8?) or unlimited?
- file conflict prevention: warn when two agents touch the same file?
  or use git worktrees / temp branches?
- should agents share tool confirmation state? (one confirms for all?)
- keyboard shortcut: ctrl+shift+enter may not be detectable in all terminals.
  need to test crossterm's keyboard enhancement protocol support. fallback
  to a slash command like /fork?

---

## completed work

<details>
<summary>phases 1-7 (all done)</summary>

### phase 1: core types + first providers
- [x] workspace setup, mush-ai core types, providers (anthropic, openai, openai-responses)
- [x] oauth, env-based api keys, model catalogue, newtypes

### phase 2: agent loop
- [x] agent tool trait, agent loop, event stream, max turns, steering, follow-up, context transforms

### phase 3: built-in tools
- [x] read, write, edit, bash, grep, find, glob, ls, web_search, web_fetch, batch

### phase 4: session management
- [x] session types, file store, save/load/list/delete, compaction, session tree, auto-save

### phase 5: extension system
- [x] extension trait, hook runner, AGENTS.md discovery, skills, templates, auto-context embeddings

### phase 6: config + CLI
- [x] clap args, print mode, stdin pipe, session resume, config file, oauth, models/sessions/status commands

### phase 7: TUI
- [x] ratatui interface, streaming, markdown, syntax highlighting, slash commands, themes
- [x] image rendering, mouse scroll, tab completion, tool confirmation, /undo, streaming bash output

</details>

<details>
<summary>refactor rounds 1-6 (all done)</summary>

- [x] enums over booleans (ToolOutcome, ThinkingContent variants, ThinkingLevel)
- [x] deduplication (AppSetup, HintMode)
- [x] async I/O (tokio::fs in tools)
- [x] newtypes (ImageMimeType, Provider in errors)
- [x] lint hygiene (#[expect], #[must_use], explicit discards)
- [x] flexible APIs (impl Into<String>), derive Default
- [x] non_exhaustive on cross-crate enums
- [x] cargo-deny integration
- [x] text extraction helpers, shared SSE parser, TokenStats, event_handler extraction
- [x] terminal corruption fix (selective mouse tracking, process group isolation)

</details>
