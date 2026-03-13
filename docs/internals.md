# internals

core abstractions: tool registry, conversation model, and agent loop.

## tool registry

`ToolRegistry` (`mush-agent/src/tool.rs`) is an ordered map of tools
keyed by `ToolKey`. it's the single source of truth for what the agent
can call.

### ToolKey normalisation

tool names are normalised to lowercase with underscores stripped.
this means `read`, `Read`, `READ`, and `re_ad` all resolve to the same tool.
the original name (from `AgentTool::name()`) is used in API calls and display.

### registration order

tools are registered in order and iterated in that order. this means the
tool list in the system prompt matches registration order. later registrations
with the same key replace earlier ones (last-write-wins).

```
ToolRegistry::new()
  -> register_shared(tool)   // adds to order + map
  -> extend_shared(tools)    // batch register
  -> with_shared(tools)      // clone + extend (non-mutating)
  -> from_boxed(vec)         // convenience for Box<dyn AgentTool>
```

### AgentTool trait

every tool implements this trait:

```rust
trait AgentTool: Send + Sync {
    fn name(&self) -> &str;              // unique name
    fn label(&self) -> &str;             // human-readable label for UI
    fn description(&self) -> &str;       // description for the LLM
    fn parameters_schema(&self) -> Value; // JSON schema for parameters
    fn execute(&self, args: Value) -> Pin<Box<dyn Future<Output = ToolResult> + Send + '_>>;
}
```

tools are stored as `SharedTool` (`Arc<dyn AgentTool>`) so they can be
shared across panes and the agent loop without cloning.

### ToolResult

three constructors: `text(s)`, `error(s)`, `image(data, mime)`.
each carries a `Vec<ToolResultContentPart>` (text or image) and a
`ToolOutcome` (success or error). the outcome affects how the LLM
interprets the result.

### truncation middleware

after every tool execution, the agent loop runs truncation:

- **threshold**: 2000 lines or 50KB (whichever is hit first)
- **direction**: `Middle` (default) keeps head + tail, `Head` keeps start, `Tail` keeps end
- **spill file**: full output saved to `~/.local/share/mush/tool-output/` with 7-day retention
- **hint**: a terse message tells the model the output was truncated and where to find the full version

the `batch` tool sets `self_truncating = true` to avoid double truncation
(it handles its own per-item truncation internally).

### tool composition at startup

in `mush-cli/src/setup.rs`:

1. **builtin tools** are created via `builtin_tools_with_options(cwd, sink, use_patch)`
   - GPT models get `apply_patch` instead of `edit`+`write`
   - streaming bash output sink is optional
2. **skill tools** (list/describe/load) are added when `skill_loading = "lazy"`
3. **MCP tools** are loaded from configured servers
   - `dynamic_mcp = false` (default): each MCP tool registered individually
   - `dynamic_mcp = true`: only meta-tools (list/get_schemas/call) registered
4. **LSP tools** added when `[lsp] diagnostics = true`
5. all tools collected into a single `ToolRegistry` passed to the TUI/print mode

## conversation model

`ConversationState` (`mush-session/src/lib.rs`) is the canonical
conversation model. it wraps a `SessionTree` and provides the interface
between the agent loop and session persistence.

### message types

messages are defined in `mush-ai/src/types.rs`:

- `Message::User` - user messages with text and optional images
- `Message::Assistant` - model responses with text, thinking, and tool calls
- `Message::ToolResult` - tool execution results (text or image parts)

each `ToolCall` has a `ToolCallId` and `ToolName` (validated newtypes).

### SessionTree

an append-only tree structure for conversation branching:

```
root
├── user: "fix the bug"
│   ├── assistant: "I'll look at..."  ← branch 0 (original)
│   │   └── ...
│   └── assistant: "Let me try..."   ← branch 1 (created by /branch)
│       └── ...
```

each node has an `id` and `parent_id`. the tree maintains a `leaf` pointer
to the current branch tip. key operations:

- `append(message)`: add to the current branch
- `branch(from_id)`: move leaf pointer, next append creates a new branch
- `branch_with_summary(from_id, summary)`: branch + inject summary of abandoned path
- `build_context()`: walk leaf -> root to get the flat message list for the LLM

### context building

`ConversationState::context()` returns a `Vec<Message>` from the current
branch by walking from the leaf to the root. this is what gets sent to the
LLM on each turn. `context_prefix(n)` returns the first `n` messages (used
for title generation).

### display rebuild

the TUI maintains its own display state (`App.messages`) separate from
the canonical conversation. `rebuild_display()` maps the canonical tree
into display widgets. this is a one-way transform: the canonical state
is the source of truth, display is derived.

### session persistence

`SessionStore` saves sessions to `~/.local/share/mush/sessions/` as JSON.
each session includes:
- metadata: id, title, model, cwd, timestamps
- the full `SessionTree` (for branching)
- a flat messages array (for backwards compat)

## agent loop

`agent_loop()` (`mush-agent/src/agent_loop.rs`) is the core execution loop.
it streams `AgentEvent`s via `async_stream`:

```
AgentStart
  TurnStart
    MessageStart
      StreamEvent(text_delta / thinking_delta / toolcall_delta)
    MessageEnd
    ToolExecStart(name, args)
      [tool execution]
    ToolExecEnd(name, result)
    ToolOutput(content)
  TurnEnd
AgentEnd
```

### flow per turn

1. build `LlmContext` from system prompt + messages + tool definitions
2. stream the assistant response, emitting deltas
3. if the response contains tool calls:
   a. run lifecycle hooks (PreToolUse)
   b. run confirmation callback (if configured)
   c. execute each tool
   d. run truncation middleware
   e. run lifecycle hooks (PostToolUse)
   f. inject LSP diagnostics (if file-modifying tool)
   g. inject file rules (if matching glob)
   h. append tool results to conversation
   i. repeat from step 1
4. if no tool calls, the turn ends

### hooks and callbacks

the agent loop accepts several callback types via `AgentConfig`:

- **`AgentHooks`**: runtime callbacks
  - `steering`: inject a steering message before each turn
  - `follow_up`: optionally continue after the agent stops
  - `context_transform`: modify the message list before sending to the LLM
  - `tool_confirmation`: prompt user before tool execution
- **`LifecycleHooks`**: user-configured shell commands (from config.toml)
  - PreSession, PreToolUse, PostToolUse, Stop, PostCompaction
- **`DiagnosticCallback`**: LSP diagnostics after file-modifying tools
- **`DynamicContext`**: per-turn system prompt additions (repo map, etc.)

### compaction

when the conversation approaches the model's context window (95% threshold):

1. **observation masking**: replace old tool outputs with summaries (no LLM call)
2. **LLM summarisation**: if masking didn't free enough, call the LLM to summarise

the summary is placed at the beginning of the context. fork-compact
(`/fc`) branches the tree first so the original conversation is preserved.
