---
name: rlm-heartbeat
description: Manage agent-owned RLM heartbeats from the Python REPL. Use when the user asks the agent to start, create, schedule, or manage a heartbeat, unless they explicitly request the user's /heartbeat.
---

# RLM Heartbeat

RLM heartbeats are internal recurring prompts for the current agent session.
They are separate from the user's visible `/heartbeat`: these calls cannot
read, replace, pause, resume or clear that user-level heartbeat, and an id
that is not an RLM heartbeat is refused.

```python
await rlm_heartbeat.create("check test progress", interval="5m", label="tests")
await rlm_heartbeat.create("watch build", delivery_mode="follow_up")
await rlm_heartbeat.list()
await rlm_heartbeat.update("job-id", status="pause")
await rlm_heartbeat.delete("job-id")
```

## API

- `await rlm_heartbeat.list(include_inactive=False)` lists this session's RLM
  heartbeats as dicts. By default only active and paused ones;
  `include_inactive=True` also shows cancelled ones, which are kept for a day
  after they were created.
- `await rlm_heartbeat.create(instruction, interval=None, label=None,
  delivery_mode=None)` creates a recurring heartbeat and returns it. The
  default interval is every 5 minutes. Multiple RLM heartbeats may run at once;
  use labels to tell them apart. `delivery_mode` is `"steer"` (default) or
  `"follow_up"`.
- `await rlm_heartbeat.update(id, instruction=None, interval=None, label=None,
  status=None, delivery_mode=None)` updates one RLM heartbeat and returns it.
  Pass at least one field. `status` is `"pause"` or `"resume"`; pausing needs
  an active heartbeat and resuming a paused one. A new `interval` restarts the
  countdown.
- `await rlm_heartbeat.delete(id)` cancels one RLM heartbeat.

Each call raises `HostError` on bad input, such as an unknown id, an
unreadable interval or an empty instruction.

## Interval

`interval` is an amount and a unit: `"30s"`, `"5m"`, `"2h"`, `"1d"`, or words
like `"10 minutes"`; a leading `"every "` is optional. The minimum is 10
seconds. One-off times and cron expressions are refused, since a heartbeat
repeats.

## Heartbeat dict

`id`, `label`, `prompt` (the instruction), `expression` (such as
`"every 5m"`), `status` (`active`, `paused` or `cancelled`), `delivery`
(`steer` or `follow_up`), `runs`, `created_at`, `last_run_at`, `schedule` and
`source` (`rlm_heartbeat`).

## Delivery mode

Each heartbeat arrives as a prompt headed `[heartbeat: <expression> run#N]`
followed by the instruction. The delivery mode decides what happens when the
session is busy:

- `steer` (default): the prompt is steered into the running turn so it is
  seen promptly.
- `follow_up`: the prompt waits until the current turn finishes.

When the session is idle, either mode starts a new turn.

## Rules

- Use this when the user asks you to start, create, schedule or manage your own
  heartbeat without explicitly referring to `/heartbeat`.
- Use this only for agent-internal recurring checks and long-running task
  coordination.
- Do not use this to satisfy a user's request to configure `/heartbeat`; that
  is a separate user-level surface the user controls.
- Keep heartbeat instructions specific and actionable so each recurring turn
  knows exactly what to inspect or continue.
