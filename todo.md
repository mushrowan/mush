# todo

## open items
- [ ] mush-ext: dynamic tool registration from extensions
- [ ] mush-ext: provider registration from extensions
- [x] grep tool: strip newlines from pattern before passing to rg (currently blows up with "literal \\n is not allowed")
- [x] edit tool: clarify in tool description that oldText must be unique (code already errors on multi-match)
- [x] edit tool: diff preview should strip common leading whitespace (dedent to leftmost line) for readability
- [ ] add more providers: google (native), google vertex, amazon bedrock, azure openai, xai (grok), mistral, groq, deepinfra, cerebras, cohere, together ai, perplexity, github copilot, gitlab, cloudflare workers ai
- [ ] steering message editing: alt+k to edit previously-sent but unrealised steering messages. first alt+k edits the most recent queued steering message, subsequent alt+k cycles to older ones. while a message is being edited, remove it from the steering queue until it's re-submitted. lets you fix typos or refine instructions before the agent sees them

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
- first split: vertical columns (left = original, right = new agent)
- subsequent splits: add more columns
- if terminal too narrow for another column, stack panes as numbered tabs
  (status bar shows "[1] [2] [3*]" where * marks the active pane)
- each pane has its own: message list, input box, status bar
- focused pane highlighted (border colour or indicator)
- ctrl+arrow or alt+number to switch focus between panes
- a pane can be closed with /close or ctrl+w, remaining panes reflow
- status bar shows pane id/label, and flags when a background pane is
  awaiting a prompt (e.g. "pane 2: awaiting prompt")
- no hard pane limit, but practically constrained by screen width and
  token cost

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
- [x] `Pane` struct: wraps `App` state + pane id + layout rect
- [x] `PaneManager`: owns Vec<Pane>, tracks focused pane, handles layout
- [x] layout algorithm: columns first, fall back to numbered tabs when too narrow
- [x] min column width threshold (60 chars), below which panes become tabs
- [x] render loop draws each pane independently into its allocated rect
- [x] focus switching: alt+number for direct jump (alt+1..9)
- [x] tab bar rendering when in stacked mode ("[1:label*] [2:label]")
- [x] pane border styling (focused vs unfocused)
- [x] /close command (currently no-op in single pane)
- [x] single-pane mode is just PaneManager with one pane (no regression)
- [x] integrate PaneManager into runner (replace bare app/conversation/session_tree)
- [x] status bar: show background pane alerts ("pane 2: awaiting prompt")

#### phase B: forking agent sessions
- [x] ctrl+shift+enter keybinding emits SplitPane event
- [x] handler: creates new pane + branches SessionTree
- [x] new pane inherits conversation history up to fork point
- [x] new pane gets its own agent_loop() stream
- [x] multiplexing: select! over all active pane streams + terminal events
- [x] the prompt typed at fork time goes to the new pane's agent
- [x] each pane has independent model, thinking level, streaming state
- [x] pane-local /model, /compact, /undo work independently

#### phase C: inter-agent communication

key insights from communication.md research:
- **context isolation is the #1 lever**: each agent should get focused, narrow
  context rather than the full shared history. SessionTree branching already
  provides this naturally (forked agents share prefix, diverge after)
- **agent-as-tool** is the dominant delegation pattern across frameworks
  (openai agents sdk, google adk, langchain). wrap sub-agents as callable
  tools so the parent decides when/to whom to delegate via normal tool use
- **typed shared state with reducers** (langgraph pattern) beats both
  unstructured message passing and full shared memory. each field in the
  shared state has an explicit merge strategy (overwrite, append, custom)
- **structured output over free text** between agents cuts tokens ~50%
  while improving accuracy. use json envelopes not prose
- **observation masking** hides old tool outputs while keeping action history,
  since 99% of token growth is tool output. cheaper than llm summarisation
- **debate only helps when**: initial model accuracy is low (<50%), models
  are heterogeneous (different families), or the task is open-ended.
  for standard tasks, self-consistency (sample + majority vote) is cheaper
- **sycophancy is the #1 debate failure mode**: agents uncritically adopt
  peers' views. anonymise responses and use all-agents-drafting (AAD)

implementation:
- [x] `send_message` tool: agent can send structured messages to siblings
- [x] received messages injected via steering mechanism into recipient context
- [x] auto-wake idle agents when they receive a message
- [x] system prompt additions: sibling awareness ("you are pane 2 of 3")
- [x] /broadcast slash command: user sends a message to all panes
- [x] message envelope: typed struct with sender, recipient, intent, parts, task_id
- [x] shared state store: typed dict per session with reducer functions per field
- [x] context isolation: forked agents get only task-relevant context slice
- [x] observation masking: strip old tool outputs from forwarded context

#### phase D: file conflict prevention
- [x] `IsolationMode` enum: `None`, `Worktree`, `Jj`
- [x] auto-detect available modes from `.jj/` and `.git/` presence
- [x] config: `[agents] isolation = "none" | "worktree" | "jj"`
- [x] **none mode:**
  - [x] per-pane file modification tracker (set of paths written/edited)
  - [x] warn in status bar when agent modifies a file touched by another pane
  - [x] /lock <path> to claim a file, /locks to list, auto-release on pane close
  - [x] locked files: write/edit tools return error with lock owner info
- [ ] **worktree mode:**
  - [ ] on fork: `git worktree add .mush/worktrees/<pane-id> -b mush-<pane-id>`
  - [ ] forked pane's tools use worktree path as cwd instead of repo root
  - [ ] on pane close: prompt to merge/keep/discard, then `git worktree remove`
  - [ ] /merge slash command to merge a pane's worktree branch back
  - [ ] cleanup stale worktrees on startup (`.mush/worktrees/`)
- [ ] **jj mode:**
  - [ ] on fork: `jj new` to create a new change for the forked pane
  - [ ] track which jj change id belongs to which pane
  - [ ] agents told their change id in system prompt for jj-aware operations
  - [ ] on pane close: `jj squash` into parent or leave as separate change
  - [ ] /merge to squash a pane's change into the main working copy change

#### phase E: polish and UX
- [ ] pane labels (auto-generated from first prompt or user-assigned)
- [ ] aggregate /cost across all panes
- [ ] pane-specific status bar with pane id and sibling count
- [ ] ctrl+shift+arrow to resize panes
- [ ] /panes command to list all active panes and their status
- [ ] session save/resume with multi-pane state
- [ ] print mode support: -p with --panes flag for parallel agents to stdout
- [ ] model tiering: budget models for routing/classification, flagship for reasoning
- [ ] semantic caching: skip llm call when a similar query was recently answered
- [ ] per-agent cost attribution in observability/tracing

### key decisions to make during implementation
- keyboard shortcut: ctrl+shift+enter ✅ works with crossterm keyboard
  enhancement protocol. /fork as fallback for terminals without support
- should agents share tool confirmation state? (one confirms for all?)
- agent-as-tool vs peer messaging: support both, let the orchestration
  pattern decide. agent-as-tool for hierarchical, messaging for peer

### file conflict prevention (research summary)

the industry has converged on a few approaches, roughly ordered by isolation level:

**1. git worktrees (industry standard for multi-process agents)**
- each agent gets its own checked-out working directory + branch
- shared .git object store, near-instant creation
- used by: claude code (`--worktree`), cursor 2.0, openai codex, ccswarm
- pros: complete filesystem isolation, no conflicts during work
- cons: disk overhead (full working tree copy per agent), port/db conflicts,
  slow cleanup on large repos, merge pain at recombination time
- best for: separate OS processes, long-running independent tasks

**2. jj-native branching (best fit for mush)**
- since mush uses jj, agents can work on separate jj changes in the same directory
- jj treats conflicts as first-class data, not blocking errors
- two agents editing the same file creates a jj conflict that can be resolved later
- `jj new @` for each forked agent creates independent changes
- agents see each other's changes after snapshot (jj auto-snapshots on every command)
- pros: zero disk overhead, works with existing jj workflow, conflicts are data not errors
- cons: agents share the same working directory so disk state can be inconsistent
  between operations (one agent's write is visible to another agent's read)

**3. per-path advisory locks (emerging pattern)**
- lightweight file-level locks: agent claims a path before editing
- openclaw PR #29793 (feb 2026): `workspace-lock-manager.ts` with atomic locks,
  stale-lock reclaim, configurable timeout
- pros: fine-grained, low overhead, works in shared directory
- cons: requires cooperation (agents must check locks), deadlock risk

**4. detect-and-warn (simplest)**
- let agents work freely, monitor for overlapping file modifications
- warn user when two agents touch the same file in the same turn
- pros: zero friction, no false positives on read-only access
- cons: damage already done by the time you warn

**recommended approach for mush (configurable isolation modes):**

the default and the available modes depend on what VCS the project uses.
configured via `[agents]` section in config.toml or auto-detected:

- **`isolation = "none"` (default):** detect-and-warn. all agents share the
  working directory. track which files each pane modifies, warn when two panes
  touch the same file. /lock command for manual advisory locks. lowest friction,
  good enough for agents working on different parts of the codebase

- **`isolation = "worktree"`:** git worktree per forked pane. each pane gets
  `<repo>/.mush/worktrees/<pane-id>/` with its own branch from the fork point.
  complete filesystem isolation. merge back via PR-style review or sequential
  merge. works with any git repo (and jj colocated repos via the .git layer)

- **`isolation = "jj"`:** jj change per forked pane. each pane works on its own
  jj change (`jj new` at the fork point). agents share the working directory
  but their edits are tracked as separate changes. conflicts are first-class
  data in jj, resolvable later. zero disk overhead, natural fit for jj repos.
  only available when `.jj/` is detected

auto-detection: if `.jj/` exists, offer "jj" and "worktree". if `.git/` exists
(without `.jj/`), offer "worktree". otherwise, only "none" is available

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

## stretch goals
- [ ] agent-as-tool: wrap pane agents as callable tools for hierarchical delegation
