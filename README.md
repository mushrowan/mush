# 🍄 mush

minimal, extensible coding agent harness in rust

mush is a local-first coding agent with a terminal UI, tool calling, session
history, branching, compaction, MCP support, multi-pane agents, LSP integration,
and a small extension system.

```text
mush
interjection / verb

used to urge a team forward
basically: go on, move, get on with it
```

## quick shape

- `mush` opens the TUI
- `mush -p "review this"` runs in print mode
- `echo "code" | mush -p "review this"` pipes stdin
- `mush -c <session-id>` resumes a session
- built for hacking on locally, with multi-provider LLM support and unixy tools

## docs

- [configuration](docs/config.md) - config.toml reference and file locations
- [tool reference](docs/tools.md) - every tool with parameters and contracts
- [internals](docs/internals.md) - tool registry, conversation model, agent loop
- [terminal](docs/terminal.md) - keyboard, mouse, and image capability handling
- [architecture](architecture.md) - crate structure and module details

## features

- **multi-provider**: anthropic, openai, openrouter, openai-codex (extensible)
- **tool calling**: read, write, edit, bash, grep, find, glob, ls, web search/fetch, batch, apply_patch
- **session management**: save, resume, branch, undo, tree navigation, compaction
- **multi-pane**: fork agents into independent panes with file locking and messaging
- **MCP**: connect to any MCP server (stdio or HTTP), dynamic tool loading
- **LSP**: auto-detect language servers, inject diagnostics after file edits
- **retrieval**: tree-sitter repo map, embedding-based skill routing, glob-pattern rules
- **lifecycle hooks**: run linters, tests, or scripts at key points in the agent loop
- **IPC**: unix domain socket for cross-process agent discovery and communication

inspired by pi-mono, but aiming to stay smaller and simpler
