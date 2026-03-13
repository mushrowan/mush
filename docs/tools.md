# tool reference

every tool the agent can call, with parameters, output contracts, and limits.

tools are registered via the `ToolRegistry` (see [internals.md](internals.md)).
output from any tool is subject to post-execute truncation: 2000 lines or 50KB,
whichever is hit first. truncated output is saved to `~/.local/share/mush/tool-output/`
with a 7-day retention policy. the agent gets a hint pointing to the saved file.

## read

read a file's contents with 1-indexed line numbers. supports text files and
images (jpg, png, gif, webp). images are returned as base64 attachments.

| param | type | required | description |
|-------|------|----------|-------------|
| path | string | yes | file path (relative or absolute) |
| offset | integer | no | line to start from (1-indexed) |
| limit | integer | no | max lines to read |
| start_line | integer | no | first line (1-indexed), use with end_line for exact spans |
| end_line | integer | no | last line (inclusive), requires start_line |
| around_line | integer | no | centre a context window on this line |

**limits**: 2000 lines or 50KB per read (whichever is hit first).
if the file exceeds this, the output is truncated and a hint tells
the agent to use offset/limit to continue.

**mutually exclusive**: `offset`/`limit` vs `start_line`/`end_line` vs `around_line`.
combining incompatible modes returns an error.

**output format**: each line prefixed with its 1-indexed line number and a tab.
binary/image files return base64 content instead.

## write

create or overwrite a file. automatically creates parent directories.

| param | type | required | description |
|-------|------|----------|-------------|
| path | string | yes | file path |
| content | string | yes | content to write |

**output**: confirmation message with the path and byte count written.

## edit

surgical find-and-replace. the old text must match exactly (including whitespace).

| param | type | required | description |
|-------|------|----------|-------------|
| path | string | yes | file path |
| oldText | string | yes | exact text to find |
| newText | string | yes | replacement text |
| expected_matches | integer | no | expected number of matches (error if different) |
| start_line | integer | no | restrict search to lines from here |
| end_line | integer | no | restrict search to lines up to here |

**output**: confirmation with match count and line numbers.
fails if oldText is not found or if expected_matches doesn't match actual count.

## apply_patch

codex-style patch format. supports add, update, delete, and move operations.

| param | type | required | description |
|-------|------|----------|-------------|
| patch_text | string | yes | patch in `*** Begin Patch` / `*** End Patch` format |

**matching strategy** (multi-pass, in order):
1. exact match
2. trailing whitespace stripped
3. both sides trimmed
4. unicode-normalised

supports `@@` context-seeking lines and end-of-file anchoring.
only available for GPT models (gpt-*, excluding gpt-4 and oss variants).

## bash

execute a shell command via `bash -c`. returns stdout and stderr.

| param | type | required | description |
|-------|------|----------|-------------|
| command | string | yes | bash command to run |
| timeout | integer | no | timeout in seconds (default 120, max 120) |

**output format**: combined stdout/stderr. exit code included on non-zero.
the command runs in its own process group for clean signal handling.

**streaming**: when streaming output is enabled, partial output is forwarded
to the UI as it arrives instead of buffering to completion.

## grep

search file contents using ripgrep. respects .gitignore.

| param | type | required | description |
|-------|------|----------|-------------|
| pattern | string | yes | regex pattern (or literal if mode=literal) |
| path | string | no | directory or file to search (defaults to cwd) |
| include | string | no | glob for files to include (e.g. `*.rs`) |
| mode | string | no | `regex` (default) or `literal` |
| case_sensitive | boolean | no | default true |
| whole_word | boolean | no | default false |
| context_before | integer | no | lines before each match (default 0) |
| context_after | integer | no | lines after each match (default 0) |
| output | string | no | `lines` (default), `count`, `files`, or `json` |
| max_results | integer | no | cap files shown in count/files mode |
| top_n | integer | no | top N files by match count (count/json modes) |

**output modes**:
- `lines`: full matching lines with file:line prefix
- `count`: per-file match counts, sorted descending
- `files`: just filenames with matches
- `json`: structured per-file counts as JSON array

## find

search for files by name pattern using fd (regex). respects .gitignore.

| param | type | required | description |
|-------|------|----------|-------------|
| pattern | string | yes | regex pattern for filename |
| path | string | no | directory to search in (defaults to cwd) |
| type | string | no | `file`, `directory`, or `any` |

## glob

fast file pattern matching using glob syntax. respects .gitignore.

| param | type | required | description |
|-------|------|----------|-------------|
| pattern | string | yes | glob pattern (e.g. `**/*.rs`) |
| path | string | no | directory to search in (defaults to cwd) |
| type | string | no | `file`, `directory`, or `any` |

## ls

list files and directories. shows file sizes and types.

| param | type | required | description |
|-------|------|----------|-------------|
| path | string | no | directory to list (defaults to cwd) |

**output**: directories listed first, then files. each entry shows type and size.

## web_search

search the web using Exa AI. returns content from the most relevant results.

| param | type | required | description |
|-------|------|----------|-------------|
| query | string | yes | search query |

**implementation**: connects to an Exa MCP server via SSE. no API key needed.

## web_fetch

fetch content from a URL. supports web pages, APIs, and images.

| param | type | required | description |
|-------|------|----------|-------------|
| url | string | yes | URL to fetch |
| format | string | no | `markdown` (default), `text`, or `html` |
| timeout | integer | no | timeout in seconds (default 30, max 120) |
| headers | object | no | custom HTTP headers |

**limits**: 5MB max response size, 50K chars max output. HTML converted to
markdown via htmd when format is `markdown`.

## batch

execute multiple tool calls concurrently. reduces latency for independent operations.

| param | type | required | description |
|-------|------|----------|-------------|
| tool_calls | array | yes | array of `{ tool, parameters }` objects |

**limits**: max 25 calls per batch. 100KB total output budget, items beyond
the budget shown as one-line summaries. partial failures are tolerated (failed
items report their error, successful items report normally).

**good for**: reading multiple files, grep+glob combos, multiple bash commands.
**bad for**: operations that depend on prior output, ordered stateful mutations.

not registered when the model supports native parallel tool calls.

## notify_user

send a desktop notification. appears outside the terminal.

| param | type | required | description |
|-------|------|----------|-------------|
| title | string | yes | notification title |
| body | string | yes | notification body |

## skill tools

three tools for lazy skill routing. registered when `skill_loading = "lazy"` (default).

### list_skills

list available skills with names and one-line descriptions. no parameters.

### describe_skill

get full description and metadata for a named skill.

| param | type | required | description |
|-------|------|----------|-------------|
| name | string | yes | skill name (case-insensitive) |

### load_skill

read the full SKILL.md content for a named skill.

| param | type | required | description |
|-------|------|----------|-------------|
| name | string | yes | skill name (case-insensitive) |

## MCP meta-tools

three tools for dynamic MCP tool access. registered when `dynamic_mcp = true`.

### mcp_list_tools

list available MCP tools with names and descriptions. optional server filter.

| param | type | required | description |
|-------|------|----------|-------------|
| server | string | no | filter to a specific MCP server |

### mcp_get_schemas

get full JSON schemas for selected MCP tools.

| param | type | required | description |
|-------|------|----------|-------------|
| tools | array | yes | array of tool name strings |

### mcp_call_tool

call any MCP tool by name with arguments.

| param | type | required | description |
|-------|------|----------|-------------|
| tool | string | yes | tool name |
| arguments | object | no | tool arguments |

## delegate_task

spawn a new agent pane to work on a sub-task independently. the sub-agent
gets its own conversation and tools. results come back via the messaging system.

| param | type | required | description |
|-------|------|----------|-------------|
| task | string | yes | the task description (becomes the sub-agent's prompt) |
| task_id | string | no | identifier for tracking (auto-generated if not provided) |

**output**: confirmation with the task_id. the sub-agent starts working
immediately and sends results back as an inter-pane message.

always available (spawns a new pane even from single-pane mode).

## lsp_diagnostics

get type errors and warnings for a file from the language's LSP server.
registered when `[lsp] diagnostics = true`.

| param | type | required | description |
|-------|------|----------|-------------|
| path | string | yes | file path to diagnose |

**output**: diagnostics formatted as `file:line:col: severity: message`, one per line.
also auto-injected after write/edit/apply_patch as `[LSP diagnostics]` addendum.
