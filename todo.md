# todo2

## why this exists
- the current tool interface makes agent-side coding brittle
- the main pain is needing exact text for `edit` while tool outputs are truncated or loosely structured
- goal is to make tools a stable frontend for any model, without needing rg-specific knowledge

## 1) add gpt-5.4 to codex model options

### what
- add `gpt-5.4-codex` to `openai_codex_models()` in `crates/mush-ai/src/models.rs`
- keep naming consistent with existing codex entries, including the chatgpt subscription label style
- confirm whether `gpt-5.4-codex` should also appear in any api-key model list, or codex oauth only
- check if default/help text mentions only `gpt-5.3-codex` and update where needed
  - likely spots: `crates/mush-cli/src/commands.rs`, docs references in `AGENTS.md`

### why
- user requested parity with opencode/pi behaviour
- codex users need the latest codex family option in the picker and config flow

### verify
- `mush models` shows the new codex model
- selecting it works in both tui and print mode
- no duplicate ids or broken provider/api mapping

## 2) make grep a model-friendly frontend over rg

### what
- keep rg backend for speed, but hide rg mental model from callers
- redesign grep params around intent, not cli flags
- proposed schema
  - `query: string` required
  - `path?: string`
  - `include?: string`
  - `mode?: "literal" | "regex"` default `literal`
  - `case_sensitive?: boolean` default `false`
  - `whole_word?: boolean` default `false`
  - `context_before?: integer` default `0`
  - `context_after?: integer` default `0`
  - `max_matches?: integer` optional hard cap
  - `output?: "text" | "json"` default `json`
- map these cleanly to rg args internally
- add deterministic json output shape
  - `meta`: query, mode, path, include, truncated, total_matches, returned_matches
  - `matches[]`: path, line, column, text

### why
- models should express search intent directly
- avoids rg flag knowledge and regex-engine confusion
- structured output enables reliable follow-up edits and tooling chains

### verify
- existing usage still works or has a clean migration path
- json output is stable and parseable
- clear error for invalid regex only when `mode=regex`

## 3) improve read for precise edits and less guesswork

### what
- keep current behaviour for simple text read, but add structured metadata option
- proposed schema additions
  - `output?: "text" | "json"` default `text` for compatibility
  - `start_line?: integer` and `end_line?: integer` as an explicit range mode
  - `around_pattern?: string`, `before?: integer`, `after?: integer` for contextual reads
- include truncation metadata when possible
  - `truncated`, `total_lines`, `returned_lines`, `total_bytes`, `returned_bytes`
- keep image support intact

### why
- exact line/range retrieval reduces fragile copy-paste edits
- metadata helps the agent know when it is missing context

### verify
- range reads are 1-indexed and documented
- conflicting params return clear errors
- json output includes enough info to decide whether another read is needed

## 4) improve bash output contract

### what
- keep command execution as-is, but optionally return structured result
- proposed schema addition
  - `output?: "text" | "json"` default `text`
- json fields
  - `stdout`, `stderr`, `exit_code`, `timed_out`, `truncated`
  - `stdout_lines`, `stderr_lines`, `stdout_bytes`, `stderr_bytes`
- make truncation explicit in both text and json modes

### why
- avoids brittle parsing of mixed stdout/stderr text
- easier for agents to branch on exit code and timeout status

### verify
- previous text format remains usable
- json consumers can reliably detect failures and truncation

## 5) consider a patch tool for robust code edits

### what
- keep strict `edit` for exact-match replacements
- add new `patch` tool that applies unified diffs or line-range replacements
- suggested minimal schema
  - `path: string`
  - `patch: string` (unified diff for one file)
  - `dry_run?: boolean`
- return clear apply diagnostics
  - applied hunks, rejected hunks, reject snippets

### why
- exact string replacement is safe but fails often in long or reformatted files
- patch-based edits are the common denominator across coding agents

### verify
- dry-run shows what would change
- failed hunks report enough context to retry automatically

## 6) docs and architecture updates after tool changes

### what
- update `AGENTS.md` built-in tool section to reflect real schema and output behaviour
- document new defaults and json contracts
- include short examples per tool with intent-style usage
- if architecture shape changes, update architecture section in `AGENTS.md`

### why
- current docs are higher-level than actual contracts
- keeping docs aligned prevents model drift and bad tool calls

### verify
- docs mention current params exactly
- examples match tested behaviour

## 7) investigate: "error decoding response body"

### what
- track and debug intermittent decode failures (seen in harness output)
- inspect likely paths
  - `mush-ai` providers: `openai`, `openai_responses`, `anthropic`, shared sse parser
  - `mush-tools` web tools: `web_fetch`, `web_search`
- improve error diagnostics
  - include status code, content-type, and bounded response snippet
  - distinguish transport error vs json decode error vs sse framing error

### why
- currently too opaque to root-cause quickly
- better diagnostics reduce ghost-chasing and false caching assumptions

### verify
- forced malformed responses produce actionable errors
- no secrets leaked in diagnostics

## 8) opencode/pi comparison pass before finalising interfaces

### what
- review how opencode/pi expose search and file-read tools to models
- extract interface ideas, not 1:1 implementation
- explicitly decide where mush diverges

### why
- user requested parity direction
- avoids reinventing bad patterns and helps with model portability

### verify
- short design note recorded with decisions and tradeoffs

## suggested order
1. gpt-5.4 codex model entry
2. grep schema + json output
3. read range/context + metadata
4. bash json contract
5. docs refresh
6. decode-error diagnostics pass
7. patch tool exploration


## newtype audit

### high priority
- [ ] token count type inconsistency: `needs_compaction()` takes `usize`, `Model.context_window` is `u64`, `auto_compact()` takes `u64`, `estimate_tokens()` returns `usize`
  - token counts flow as both `u64` (API responses, Model) and `usize` (estimate_tokens, needs_compaction)
  - implicit `as` casts between them
  - fix: `TokenCount(u64)` newtype in mush-ai, use consistently everywhere
- [ ] duplicate compaction threshold logic: `needs_compaction()` uses 75% estimate-based, `auto_compact()` uses 95% usage-based
  - two independent systems checking different things with different thresholds and different token counting
  - fix: unify into a single `CompactionPolicy` or at least use the same token type and make the two thresholds explicit/documented

### medium priority
- [ ] `session_id: Option<String>` in `StreamOptions` and `ExtensionContext` but `SessionId` newtype exists in mush-session
  - newtype lives in the wrong crate, mush-ai and mush-ext can't use it
  - fix: move `SessionId` to mush-ai::types (all crates depend on it), use everywhere
- [ ] `Cost` fields are bare `f64`: `Cost { input: f64, .. }`, `ModelCost { input: f64, .. }`, `TokenStats.total_cost: f64`
  - easy to mix "cost per million tokens" with "actual cost in dollars"
  - `$0.0042` display formatting repeated in status bar and slash commands
  - fix: `Dollars(f64)` newtype with Display, or separate `CostPerMillion(f64)` and `Dollars(f64)`
- [ ] `context_window: u64` and `max_output_tokens: u64` on Model are interchangeable bare `u64`
  - easy to pass one where the other is expected
  - fix: if doing `TokenCount` newtype above, both become `TokenCount` which is fine (same domain), or distinct newtypes if we want compiler-enforced separation

### low priority
- [ ] `truncate_with_ellipsis` in compact.rs scans string twice (`chars().count()` then re-iterates)
  - not a newtype issue but could use `char_indices` to do it in one pass

## in progress (this session)
- [x] fix: pane layout panic when terminal narrower than MIN_COLUMN_WIDTH (clamp min > max)
- [x] fix: compaction never re-triggers after initial compact (cache replay never re-checks needs_compaction)
- [x] fix: ContextTransformed event fires every turn during cache replay, inflating status bar counts
- [x] feat: retry logic for transient API errors (network, 429, 5xx) with exponential backoff
- [x] fix: edit tool fails matching text with trailing whitespace in files (blocked tooling, needs investigation)
  - the Edit tool's exact-match approach chokes when file lines have trailing whitespace that isn't visible in Read output
  - workaround: use python/sed for replacements, but this defeats the purpose
  - consider: normalise trailing whitespace in match candidates, like we already do for CRLF/LF

## needs verification
- [x] compaction: test that re-compaction actually fires when compacted+new messages exceed threshold
  - the auto_compact function uses both usage-based and estimate-based checks
  - need to verify context_tokens_shared is being read correctly after the cache replay change
- [x] retry: test that retries actually work in practice (hard to unit test network failures)
  - currently retries up to 3 times with 1s/2s/4s backoff
  - only retries reqwest errors and 429/5xx status codes
- [ ] status bar: verify "compacted x → y" only shows on actual new compaction, not cache replays

## open items
- [ ] mush-ext: dynamic tool registration from extensions
- [ ] mush-ext: provider registration from extensions
- [x] grep tool: strip newlines from pattern before passing to rg (currently blows up with "literal \\n is not allowed")
- [x] edit tool: near-miss diagnostics (whitespace diff, similar line suggestions, match locations)
- [x] edit tool: trailing newline normalisation (try with/without trailing \n)
- [x] read tool: report empty files explicitly instead of blank string
- [x] web_fetch: char boundary safe truncation (floor_char_boundary)
- [x] tui: fix abort blocking new message submissions
- [x] tui: notification sounds via pw-play + freedesktop theme
- [x] tui: show token counts in compact status message
- [x] tools: add notify_user tool for desktop notifications
- [x] session: LLM-based title generation after first turn
- [x] edit tool: clarify in tool description that oldText must be unique (code already errors on multi-match)
- [x] edit tool: diff preview should strip common leading whitespace (dedent to leftmost line) for readability
- [ ] add more providers: google (native), google vertex, amazon bedrock, azure openai, xai (grok), mistral, groq, deepinfra, cerebras, cohere, together ai, perplexity, github copilot, gitlab, cloudflare workers ai
- [x] steering message editing: alt+k to edit queued steering messages

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
- [x] **worktree mode:**
  - [x] on fork: `git worktree add .mush/worktrees/<pane-id> -b mush-<pane-id>`
  - [x] forked pane's tools use worktree path as cwd instead of repo root
  - [x] on pane close: prompt to merge/keep/discard, then `git worktree remove`
  - [x] /merge slash command to merge a pane's worktree branch back
  - [x] cleanup stale worktrees on startup (`.mush/worktrees/`)
- [x] **jj mode:**
  - [x] on fork: `jj new` to create a new change for the forked pane
  - [x] track which jj change id belongs to which pane
  - [x] agents told their change id in system prompt for jj-aware operations
  - [x] on pane close: `jj abandon` to discard the change
  - [x] /merge to squash a pane's change into the parent change

#### phase E: polish and UX
- [x] pane labels (auto-generated from first prompt or user-assigned)
- [x] aggregate /cost across all panes
- [x] pane-specific status bar with pane id and sibling count
- [x] /panes command to list all active panes and their status
- [x] ctrl+shift+arrow to resize panes
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
