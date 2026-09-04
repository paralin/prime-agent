---
name: agent-observe
description: Read-only roster and observation of an agent's parent, siblings, and direct children. Use to discover reachable agents and to inspect family status and bounded recent-message previews without mutating sessions.
---

# Agent Observe

Observe the current agent's nuclear family through the local daemon: parent,
siblings, direct children, and self. `list_agents` is the one family roster and
covers every member `agent_message.send` can reach. `get_agent` and
`recent_messages` hydrate an inactive child before reading it. They cannot read
a root sibling that has no live session in this worker.
This skill is read-only: it can list family sessions, inspect one session, and fetch
bounded recent message previews. It cannot prompt, steer, clear, kill, rename, or
otherwise mutate another session.

This read-only skill lists reachable sessions, inspects one session, and fetches
bounded recent-message previews. Parent-side lifecycle and messaging operations
perform every session mutation.

Call directly from the IPython kernel:

```python
children = await rlm.list_subagents()
child = next(iter(children), None)
if child is not None:
    observed = await agent_observe.get_agent(child.session_name)
    recent = await agent_observe.recent_messages(child.session_name, limit=6)
```

`rlm.list_subagents()` returns direct-child registry entries. Use
`child.session_name` to address one of those entries. Child deletion remains a
separate parent-side `rlm.delete_subagent(target)` operation. The current
`target` contract accepts a string selector or an `RLMSubagent` registry entry.

## API

- `await agent_observe.list_agents()` returns `current` and `agents`, the full
  nuclear family: parent, siblings, and direct children, active or not. Each
  agent carries `sessionId`, optional `sessionName`, `relationship`
  (`parent`/`sibling`/`child`), `status`, `isSessionActive`, and the counts and
  message previews known for it: `latestMessage` for a live session,
  `firstMessage` for an inactive child. A member with no live session has
  no `activeSessionId` and no live detail; address it with `agent_message.send`
  using its `relationship` plus its `sessionName`, or its `sessionId` when the
  member has no name. For direct children,
  `await rlm.list_subagents()` also exposes parent-owned lifecycle handles.
- `await agent_observe.get_agent(target)` returns `agent`, where `agent`
  contains one live agent summary. `target` is resolved like other live-session
  selectors: active id, session id/name, or unambiguous suffix.
- `await agent_observe.recent_messages(target, limit=8, max_chars=800)`
  returns up to `limit` recent bounded message previews for the target session.
  `limit` must be 1-50, and `max_chars` must be 80-2000.

## Boundaries

- Targets outside the current agent, parent, siblings, and direct children are
  rejected. Transcript reads follow the same reach boundary as messaging.
- Message access is bounded by count and by the character limit for each
  preview.
- Observation grants no additional authority. Use observed information only for
  the current task and under the same mutation, communication, privacy, and
  publication rules that already govern the observing agent.
