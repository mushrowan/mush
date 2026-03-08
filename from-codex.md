# from codex

quick handoff for tools work

## what i just did
- reverted these files to committed state
  - `crates/mush-tools/src/read.rs`
  - `crates/mush-tools/src/edit.rs`
  - `crates/mush-tools/src/grep.rs`
- left `todo.md` change in place
  - added `## response stitching glitch` section

## current working copy
- only `todo.md` is modified
- no in-progress tool mutations left

## goal
improve tooling ergonomics without losing safety

## agreed scope
1) truncation guidance improvements
2) read exact span helpers
3) grep structured summaries
4) apply_patch tool
5) edit improvements

## recommended order
1. `read` exact spans
2. `grep` top summaries
3. `edit` expected matches and range-limited search
4. `apply_patch` v1
5. truncation footer improvements in `mush-agent/src/truncation.rs`

this order gives fast wins and lower risk

## details by item

### 1) read exact span helpers
file: `crates/mush-tools/src/read.rs`

add schema fields
- `start_line` integer
- `end_line` integer
- `around_line` integer
- `context_before` integer default 20
- `context_after` integer default 20

behaviour
- if `around_line` is set, derive `offset` and `limit`
- if `start_line` is set, use exact window
- if `end_line` without `start_line`, return error
- if `end_line < start_line`, return error
- keep existing truncation behaviour and json metadata

tests to add
- start/end window returns expected slice
- around_line window returns expected slice
- invalid combos return error


### 2) grep structured summaries
file: `crates/mush-tools/src/grep.rs`

add schema field
- `top_n` integer

behaviour
- for `output: "count"` and `output: "json"`, sort by count desc then apply `top_n`
- if `top_n` absent, keep current behaviour
- keep `max_results` behaviour as fallback when `top_n` not provided

tests to add
- count mode respects `top_n`
- json mode respects `top_n`


### 3) edit improvements
file: `crates/mush-tools/src/edit.rs`

add schema fields
- `expected_matches` integer default 1
- `start_line` integer
- `end_line` integer

behaviour
- match counting should respect optional line range
- edit only proceeds when found count == expected_matches
- error should report expected vs actual count
- if range is used, include range in error text
- if `end_line < start_line`, return error

important
- do this with small targeted edits only
- avoid broad scripted replacement in this file

tests to add
- expected_matches > 1 works
- mismatched expected_matches errors clearly
- range-limited edit only affects selected span
- invalid line range errors


### 4) apply_patch tool
new file: `crates/mush-tools/src/apply_patch.rs`
wire in `crates/mush-tools/src/lib.rs`

v1 proposal
- accept unified diff text in `patch`
- support file add/update/delete for tracked cwd paths
- reject absolute paths and parent traversal
- return summary of applied hunks and files changed

safer fallback if full patch parser is too big
- minimal format with explicit ops
  - `{ op: "replace", path, old_text, new_text }`
  - `{ op: "create", path, content }`
  - `{ op: "delete", path }`
- still call it apply_patch and keep schema explicit


### 5) truncation guidance improvements
file: `crates/mush-agent/src/truncation.rs`

keep caps as-is first
- lines: 2000
- bytes: 50kb

improve footer text for truncated outputs
- always include saved full output path
- include exact follow-up suggestions
  - `read` with `start_line` and `end_line`
  - `grep` suggestion to narrow first
- include clear stats
  - shown lines
  - remaining lines
  - shown bytes
  - remaining bytes


## verification checklist
- `cargo check -p mush-tools`
- `cargo test -p mush-tools`
- optional `cargo clippy -p mush-tools --all-targets -- -W clippy::pedantic`


## known pitfall from this session
- broad scripted replacements can mutate unrelated parts
- especially risky in `edit.rs`
- prefer `read` + small `edit` calls with exact context


## nice-to-have after main scope
- add `read` metadata for exact request echo in json output
  - requested start/end
  - resolved start/end
- add `grep` secondary sort by file path for stable output


good luck, we got this