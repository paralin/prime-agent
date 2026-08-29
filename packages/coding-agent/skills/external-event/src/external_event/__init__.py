"""Event-driven wake-ups for retained Prime Agent kernel work."""

from __future__ import annotations

import asyncio
import math
import secrets
from dataclasses import dataclass
from typing import Any, Literal

from rlm import BashHandle, BashResult, host_request

DeliveryPolicy = Literal["followUp", "steer"]
_MAX_COMPLETED_JOBS = 256
_MAX_ACTIVE_JOBS = 128
_MAX_LABEL_CHARS = 128
_MAX_EVENT_TEXT_CHARS = 32_000
_MAX_COMMAND_CHARS = 2_000


@dataclass(frozen=True)
class JobInfo:
    id: str
    label: str
    pid: int
    command: str
    status: str
    exit_code: int | None
    duration: float | None
    notification_status: str | None
    notification_error: str | None


@dataclass
class _WatchedJob:
    id: str
    label: str
    handle: BashHandle
    tail_lines: int
    delivery_policy: DeliveryPolicy
    task: asyncio.Task[None] | None = None
    status: str = "running"
    result: BashResult | None = None
    notification_status: str | None = None
    notification_error: str | None = None
    cancellation_requested: bool = False

    def info(self) -> JobInfo:
        return JobInfo(
            id=self.id,
            label=self.label,
            pid=self.handle.pid,
            command=_command_preview(self.handle.command),
            status=self.status,
            exit_code=self.result.exit_code if self.result is not None else None,
            duration=self.result.duration if self.result is not None else None,
            notification_status=self.notification_status,
            notification_error=self.notification_error,
        )


_jobs: dict[str, _WatchedJob] = {}


def _field(value: Any, name: str, max_chars: int) -> str:
    if not isinstance(value, str):
        raise TypeError(f"{name} must be str, got {type(value).__name__}")
    value = value.strip()
    if not value:
        raise ValueError(f"{name} cannot be empty")
    if len(value) > max_chars:
        raise ValueError(f"{name} is too long: maximum is {max_chars} characters")
    return value


def _policy(value: Any) -> DeliveryPolicy:
    if not isinstance(value, str):
        raise TypeError(f"delivery_policy must be str, got {type(value).__name__}")
    if value not in {"followUp", "steer"}:
        raise ValueError('delivery_policy must be "followUp" or "steer"')
    return value


async def emit(
    name: str,
    event_id: str,
    text: str,
    delivery_policy: DeliveryPolicy = "followUp",
) -> dict[str, Any]:
    """Admit one identified event and wake this session through its input owner."""
    name = _field(name, "name", _MAX_LABEL_CHARS)
    event_id = _field(event_id, "event_id", 512)
    if not isinstance(text, str):
        raise TypeError(f"text must be str, got {type(text).__name__}")
    if not text.strip():
        raise ValueError("text cannot be empty")
    if len(text) > _MAX_EVENT_TEXT_CHARS:
        raise ValueError(f"text is too long: maximum is {_MAX_EVENT_TEXT_CHARS} characters")
    return await host_request(
        "session.external_event.emit",
        {
            "name": name,
            "event_id": event_id,
            "text": text,
            "delivery_policy": _policy(delivery_policy),
        },
    )


def watch_bash(
    job: BashHandle,
    label: str,
    *,
    tail_lines: int = 40,
    delivery_policy: DeliveryPolicy = "followUp",
) -> str:
    """Retain a BashHandle and emit one bounded terminal event after it exits."""
    if not isinstance(job, BashHandle):
        raise TypeError(f"job must be BashHandle, got {type(job).__name__}")
    label = _field(label, "label", _MAX_LABEL_CHARS)
    if isinstance(tail_lines, bool) or not isinstance(tail_lines, int):
        raise TypeError(f"tail_lines must be int, got {type(tail_lines).__name__}")
    if tail_lines < 0 or tail_lines > 1_000:
        raise ValueError("tail_lines must be between 0 and 1000")
    policy = _policy(delivery_policy)
    for watched in _jobs.values():
        if watched.handle is job:
            raise ValueError(f"job is already watched as {watched.id}")
    if sum(watched.status == "running" for watched in _jobs.values()) >= _MAX_ACTIVE_JOBS:
        raise RuntimeError(f"too many active watched jobs: maximum is {_MAX_ACTIVE_JOBS}")
    loop = asyncio.get_running_loop()
    job_id = secrets.token_hex(16)
    watched = _WatchedJob(job_id, label, job, tail_lines, policy)
    _jobs[job_id] = watched
    # Reading pid deliberately marks the handle as retained/background before
    # the watcher starts, so cancellation of this task cannot abandon ownership.
    watched.info()
    watched.task = loop.create_task(_finish(watched), name=f"external-event-bash-{job_id}")
    return job_id


async def _finish(job: _WatchedJob) -> None:
    error: BaseException | None = None
    try:
        job.result = await job.handle
    except TimeoutError as caught:
        error = caught
        job.result = job.handle.poll()
    except Exception as caught:
        error = caught
        job.result = job.handle.poll()

    if job.cancellation_requested:
        job.status = "cancelled"
    elif isinstance(error, TimeoutError):
        job.status = "timed_out"
    elif error is not None and job.result is None:
        job.status = "failed"
    else:
        job.status = "completed"

    try:
        receipt = await emit("bash", job.id, _completion_text(job, error), job.delivery_policy)
        status = receipt.get("deliveryStatus")
        job.notification_status = status if isinstance(status, str) else "accepted"
    except Exception as caught:
        job.notification_error = f"{type(caught).__name__}: {caught}"
    finally:
        _trim_completed()


def _command_preview(command: str) -> str:
    if len(command) <= _MAX_COMMAND_CHARS:
        return command
    return command[: _MAX_COMMAND_CHARS - 1] + "…"


def _completion_text(job: _WatchedJob, error: BaseException | None) -> str:
    result = job.result
    command = _command_preview(job.handle.command)
    fields = [
        f"Bash job {job.label!r} {job.status}.",
        f"Job ID: {job.id}",
        f"PID: {job.handle.pid}",
        f"Command: {command}",
        f"Exit code: {result.exit_code if result is not None else 'unknown'}",
        f"Duration: {result.duration:.3f}s" if result is not None and math.isfinite(result.duration) else "Duration: unknown",
    ]
    if error is not None:
        fields.append(f"Error: {type(error).__name__}: {error}")
    if job.tail_lines:
        lines = job.handle.tail().splitlines()
        tail = "\n".join(lines[-job.tail_lines :])
        if tail:
            available = _MAX_EVENT_TEXT_CHARS - len("\n".join(fields)) - len("\nOutput tail:\n")
            if available <= 0:
                return "\n".join(fields)
            if len(tail) > available:
                tail = "…" if available == 1 else "…" + tail[-(available - 1) :]
            fields.extend(("Output tail:", tail))
    return "\n".join(fields)


def _trim_completed() -> None:
    completed = [job_id for job_id, job in _jobs.items() if job.status != "running"]
    for job_id in completed[:-_MAX_COMPLETED_JOBS]:
        del _jobs[job_id]


def list_jobs() -> list[JobInfo]:
    """Return active and recently completed watched jobs, oldest first."""
    return [job.info() for job in _jobs.values()]


def get_job(job_id: str) -> JobInfo | None:
    """Return one watched job, or None when it is not retained."""
    job_id = _field(job_id, "job_id", 512)
    job = _jobs.get(job_id)
    return job.info() if job is not None else None


async def cancel_job(job_id: str) -> JobInfo:
    """Terminate one watched local process group and await its completion event."""
    job_id = _field(job_id, "job_id", 512)
    job = _jobs.get(job_id)
    if job is None:
        raise KeyError(f"unknown external-event job: {job_id}")
    if job.status == "running":
        job.cancellation_requested = True
        job.handle.kill()
    assert job.task is not None
    await asyncio.shield(job.task)
    return job.info()


__all__ = ["JobInfo", "cancel_job", "emit", "get_job", "list_jobs", "watch_bash"]
