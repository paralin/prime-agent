---
name: external-event
description: Emit a completion event to the current Prime Agent session or retain a BashHandle and wake the session exactly once when it exits. Use for slow event-driven work that should resume this agent without polling.
---

# External Event

Use event-driven completion for work that should outlive the current turn.

```python
job = bash("long command")
job_id = external_event.watch_bash(job, "video capture")
```

End the turn after registration. When the command exits, Prime Agent receives
one follow-up with the label, command, PID, exit code, duration, transport
status, remote host/cwd/env-key metadata when present, and bounded output tail.

## API

- `await external_event.emit(name, event_id, text, delivery_policy="followUp")`
  admits one identified event. Reusing the same name and event ID coalesces it.
- `external_event.watch_bash(job, label, tail_lines=40, delivery_policy="followUp")`
  registers a retained `BashHandle` before returning and returns its job ID.
- `external_event.list_jobs()` returns active and recently completed jobs.
- `external_event.get_job(job_id)` returns one job or `None`.
- `await external_event.cancel_job(job_id)` terminates that job's local process
  group and waits for its completion event.

The registry is owned by the live kernel and survives model turns and native
compaction. A kernel restart terminates local Bash jobs; admitted completion
events remain in session history. `cancel_job()` controls only the local process
group. It does not stop a detached remote `tmux` session. Stop the named remote
session explicitly before closing its local SSH waiter. Sessions started in
`rpc-only` harness mode do not admit external events; the retained job record
then reports the host error in `notification_error` instead of waking a turn.
