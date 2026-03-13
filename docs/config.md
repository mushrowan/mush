# configuration

mush reads `~/.config/mush/config.toml` at startup. all fields are optional.
hot-reload is supported for theme changes.

## example config

```toml
# model and generation
model = "claude-sonnet-4-20250514"
thinking = "high"                  # off | low | medium | high
max_tokens = 16384
max_turns = 50

# context and caching
cache_retention = "long"           # none | short | long
auto_compact = true                # auto-compact when approaching context limit
auto_fork_compact = false          # fork before auto-compacting (preserves original)

# behaviour
confirm_tools = false              # prompt before tool execution
show_cost = false                  # dollar cost in status bar (toggle with /cost)
cache_timer = false                # cache warmth countdown + desktop notifications
hint_mode = "full"                 # full | short | off

# display
thinking_display = "collapse"      # hidden | collapse | expanded
log_filter = "mush=info,warn"     # tracing filter string

# multi-pane
isolation = "none"                 # none | worktree | jj

# MCP
dynamic_mcp = false                # use meta-tools instead of loading all MCP schemas

# custom system prompt (appended to the default)
# system_prompt = "always use british spelling"

[api_keys]
anthropic = "sk-ant-..."
openrouter = "sk-or-..."
openai = "sk-..."

[terminal]
keyboard_enhancement = "auto"      # auto | enabled | disabled
mouse_tracking = "minimal"         # minimal | disabled
image_probe = "auto"               # auto | disabled

[retrieval]
repo_map = true                    # tree-sitter repo map in system prompt
embeddings = true                  # embedding-based skill/tool matching
context_budget = 2048              # token budget for retrieval context
skill_loading = "lazy"             # lazy | eager
embedding_model = "coderank"       # coderank | gemma
auto_load_threshold = 0.5          # cosine similarity for auto-loading skills

[lsp]
diagnostics = true                 # auto-inject diagnostics after file edits

[lsp.servers.rust]
command = "rust-analyzer"
args = []

[lsp.servers.python]
command = "pyright-langserver"
args = ["--stdio"]

[theme]
# see theme section below

# MCP servers
[mcp.filesystem]
command = "npx"
args = ["-y", "@anthropic/mcp-server-filesystem", "/home/user/project"]

[mcp.remote-api]
url = "https://api.example.com/mcp"

# lifecycle hooks
[[hooks.pre_session]]
command = "echo 'session starting'"
timeout = 10

[[hooks.post_tool_use]]
match = "edit|write"
command = "cargo check 2>&1 | head -20"
timeout = 30
blocking = false

[[hooks.stop]]
command = "cargo test 2>&1 | tail -10"
blocking = true

[[hooks.post_compaction]]
command = "echo 'compacted'"
```

## field reference

### top-level

| field | type | default | description |
|-------|------|---------|-------------|
| model | string | auto-detected | model ID to use |
| thinking | string | off | thinking/reasoning level |
| max_tokens | integer | model default | max output tokens per response |
| max_turns | integer | none | max agent turns per message |
| cache_retention | string | none | prompt caching: none, short, long |
| debug_cache | bool | false | log cache hit/miss details |
| auto_compact | bool | false | auto-compact approaching context limit |
| auto_fork_compact | bool | false | fork tree before auto-compacting |
| confirm_tools | bool | false | require confirmation before tools run |
| show_cost | bool | false | show cost in status bar |
| cache_timer | bool | false | cache warmth countdown |
| isolation | string | none | multi-pane isolation: none, worktree, jj |
| dynamic_mcp | bool | false | dynamic MCP tool loading |
| system_prompt | string | none | custom system prompt addition |
| log_filter | string | none | tracing filter (e.g. "mush=debug") |
| thinking_display | string | collapse | how to show thinking: hidden, collapse, expanded |
| hint_mode | string | full | tool usage hints: full, short, off |

### [api_keys]

override API keys from config instead of environment variables.

| field | type | description |
|-------|------|-------------|
| anthropic | string | Anthropic API key |
| openrouter | string | OpenRouter API key |
| openai | string | OpenAI API key |

environment variables (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY`)
take precedence if set. OAuth tokens (starting with `ey`) are detected automatically.

### [terminal]

see [terminal.md](terminal.md) for full details.

### [retrieval]

controls the auto-context retrieval system.

| field | type | default | description |
|-------|------|---------|-------------|
| repo_map | bool | true | include tree-sitter repo map in system prompt |
| embeddings | bool | true | enable embedding-based skill matching |
| context_budget | integer | 2048 | token budget for retrieval context |
| skill_loading | string | lazy | `lazy` (on-demand tools) or `eager` (inline all) |
| embedding_model | string | coderank | `coderank` (code-specialised) or `gemma` (general) |
| auto_load_threshold | float | 0.5 | cosine similarity threshold for auto-loading |

### [lsp]

controls LSP integration for diagnostics.

| field | type | default | description |
|-------|------|---------|-------------|
| diagnostics | bool | false | enable auto-injecting diagnostics |

#### [lsp.servers.*]

override auto-detected LSP servers per language.

| field | type | required | description |
|-------|------|----------|-------------|
| command | string | yes | server command |
| args | array | no | command arguments |

supported language keys: rust, python, typescript, javascript, go, c, cpp,
nix, java, bash.

auto-detected servers (when on PATH): rust-analyzer, pyright, pylsp,
typescript-language-server, gopls, clangd, nil, nixd, jdtls, bash-language-server.

### [mcp.*]

MCP server configurations. each key is a server name used to namespace tools.

**local (stdio) servers**:
```toml
[mcp.myserver]
command = "npx"
args = ["-y", "my-mcp-server"]
```

**remote (HTTP) servers**:
```toml
[mcp.remote]
url = "https://api.example.com/mcp"
```

tools from MCP servers are registered as `servername_toolname`.

### [[hooks.*]]

lifecycle hooks run shell commands at specific points. see
[the hooks section in architecture.md](../architecture.md) for full details.

| section | when | blocking effect |
|---------|------|-----------------|
| `[[hooks.pre_session]]` | once at session start | output injected as context |
| `[[hooks.pre_tool_use]]` | before each tool | failure returns error, skips tool |
| `[[hooks.post_tool_use]]` | after each tool | output appended to tool result |
| `[[hooks.stop]]` | before agent stops | failure injects feedback, loop continues |
| `[[hooks.post_compaction]]` | after compaction | output injected as user message |

hook fields:

| field | type | default | description |
|-------|------|---------|-------------|
| match | string | `*` | tool name pattern: `*` for all, `edit\|write` pipe-separated |
| command | string | required | shell command |
| timeout | integer | 30 | timeout in seconds |
| blocking | bool | false | whether failure blocks the operation |

## file locations

| path | purpose |
|------|---------|
| `~/.config/mush/config.toml` | main config |
| `~/.local/share/mush/sessions/` | saved sessions |
| `~/.local/share/mush/models/` | downloaded embedding models |
| `~/.local/share/mush/tool-output/` | truncated tool output spill files |
| `$XDG_RUNTIME_DIR/mush/` | IPC sockets for running sessions |
| `.mush/tasks/` | task lock files (per-project) |
| `.mush/rules/*.md` | glob-pattern rule files (per-project) |
