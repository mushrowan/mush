---
name: goal
description: Read and complete the persistent thread goal from the Python REPL. Use to check goal status and budget usage, to start a goal when the user explicitly asks for one, or to mark the active goal complete once its objective is fully achieved.
---

# Goal

The thread goal is a persistent objective. While it is active, mush keeps
prompting you to continue it after every turn (`[goal: continuation N]`) until
it is complete. Goal state lives in the host; these calls are the kernel side
of it:

```python
await goal.get()
await goal.create("ship the release notes")
# only pass token_budget when the user explicitly asks for one:
# await goal.create("ship the release notes", token_budget=200000)
await goal.complete()
```

## API

- `await goal.get()` returns a dict with `goal` (`None` when no goal is set),
  `remaining_tokens` (`None` without a token budget) and
  `completion_budget_report` (always `None` here). The `goal` dict carries
  `id`, `objective`, `status` (`active`, `paused`, `budget_limited`, `complete` or
  `failed`), `token_budget`, `tokens_used`, `time_used_seconds`,
  `continuations`, `summary`, `error` (only on a failed goal) and timestamps.
- `await goal.create(objective, token_budget=None)` starts a new active goal
  and returns the same dict as `goal.get()`. `token_budget` is keyword-only and
  must be a positive integer. It raises `HostError` while a goal is still
  pending (active, paused or budget_limited); a complete or failed goal is
  replaced. The objective must be non-empty and at most 4000 characters.
  Only create a goal when the user or system/developer instructions explicitly
  ask for a persistent long-running goal; do not infer goals from ordinary
  tasks. Set `token_budget` only when an explicit token budget is requested.
- `await goal.complete(summary=None)` marks the goal complete, with an optional
  short summary, and stops the continuations. It raises `HostError` when no
  goal is set. Use it only when the objective has actually been achieved and no
  required work remains; do not call it because the budget is nearly spent or
  because you are stopping work. When the reply has a
  `completion_budget_report`, report that final usage to the user.

## Budget and failure

- `tokens_used` counts the input and output tokens of each assistant message
  while the goal is active; cache reads and writes are not counted.
- When `tokens_used` reaches `token_budget` the goal becomes `budget_limited`
  and you get one `[goal: budget limit]` prompt: stop starting new work and
  wrap up with progress, what remains, blockers and a next step.
- A turn that ends in an error fails the active goal; mush records the error.

## Rules

- Every status change other than completion (pause, resume, clear, fail,
  budget limiting) belongs to the user and the host, through `/goal pause`,
  `/goal resume`, `/goal clear` and `/goal fail <reason>`. There is no kernel
  API for them. If a goal is blocked, say so and ask the user.
- When an active goal is actually complete, call `await goal.complete()`; do
  not merely say it is done, because mush keeps continuing the goal until the
  completion call arrives.
