# 🍄 mush

local-first coding agent harness in rust

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

## highlights

- tool calling for files, shell, web, and MCP
- session history, branching, and compaction
- multi-pane agents with delegation
- LSP diagnostics after edits
- built for hacking on locally

inspired by pi-mono, but aiming to stay smaller and simpler
