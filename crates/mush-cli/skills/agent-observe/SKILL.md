---
name: agent-observe
description: Read-only roster and observation of an agent's parent, siblings, and direct children. Use to discover reachable agents and to inspect family status and bounded recent-message previews without mutating sessions.
---

# Agent Observe

Observe the current agent's nuclear family through the local daemon: parent,
siblings, direct children, and self. `list_agents` is the one family roster and
covers every member `agent_message.send` can reach. `recent_messages` reads
only members with a live session (`observable`).
This skill is read-only: it can list family sessions, inspect one session, and fetch
bounded recent message previews. It cannot prompt, steer, clear, kill, rename, or
otherwise mutate another session.

Call directly from the kernel:

```python
children = await rlm.list_subagents()
child = next((item for item in children if item.active_session_id), None)
if child is not None:
    worker = await agent_observe.get_agent(child.session_name)
    recent = await agent_observe.recent_messages(child.session_name, limit=6)
    # Deletion is a parent-owned RLM operation, not an observe mutation:
    await rlm.delete_subagent(child)
```

## API

- `await agent_observe.list_agents()` returns the full nuclear family as a
  list: self, parent, siblings, and direct children, active or not. Each agent
  carries `name`, `relationship` (`self`/`parent`/`sibling`/`child`),
  `session_id`, `rlm_child_id` for a subagent, `status`
  (`running`/`idle`/`inactive`), `state` (`working`/`napping`/`hibernating`;
  `agent_message.send("all", ...)` skips hibernating members), `observable` (whether it has a live session
  that can be read), and the live detail known for it: `model_id`,
  `progress_note`, `streaming`, `tool_running`, `awaiting_confirmation`, and
  `children_running`. Address a member with `agent_message.send` using its
  `relationship` plus its `name`. For direct children,
  `await rlm.list_subagents()` also exposes parent-owned lifecycle handles.
- `await agent_observe.get_agent(target)` returns one agent summary. `target`
  is resolved like other live-session selectors: name, session id, or subagent
  id.
- `await agent_observe.recent_messages(target, limit=8, max_chars=800)`
  returns up to `limit` recent bounded message previews for the target session,
  each with `role`, `text`, `truncated`, and `tool_calls`. `limit` is clamped
  to 1-50, and `max_chars` to 80-2000.

## Safety

- This skill is read-only and exposes no mutation commands.
- Targets outside the nuclear family are rejected; transcript reads follow the
  same family rule as messaging.
- Message access is bounded by count and per-message character limit.
- Prefer status and recent previews for orchestration. Ask the user before
  using observed context to steer or message another session.
