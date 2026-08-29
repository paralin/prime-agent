"""Shared-namespace Act execution for the CPython REPL runtime."""

from __future__ import annotations

import asyncio
import contextvars
import time
import traceback
from collections.abc import AsyncIterable, Awaitable, Callable
from dataclasses import dataclass
from typing import Any


class _ActDone(BaseException):
    """Stops the active shared cell after recording its exact result."""


class _ActInterrupted(Exception):
    """The active shared cell or its host exchange was interrupted."""


@dataclass(frozen=True)
class _ActCellResult:
    duration_ms: float
    stdout: str
    stderr: str
    result: str | None
    error: str | None


class _ActState:
    def __init__(self) -> None:
        self.value: Any = None
        self.completed = False
        self.cell_task: asyncio.Task[Any] | None = None


_active_state: contextvars.ContextVar[_ActState | None] = contextvars.ContextVar(
    "prime_agent_act_state", default=None
)


def done(value: Any) -> None:
    """Return an exact value from the active shared Act cell."""
    state = _active_state.get()
    if state is None or asyncio.current_task() is not state.cell_task:
        raise RuntimeError("rlm.done() is only valid in the active Act cell")
    if state.completed:
        raise RuntimeError("rlm.done() was already called")
    state.value = value
    state.completed = True
    raise _ActDone


def _format_error(error: BaseException) -> str:
    lines = traceback.format_exception(type(error), error, error.__traceback__)
    return "".join(lines).strip()


async def _run_cells(
    cells: AsyncIterable[str],
    cancel: Awaitable[object] | None = None,
    on_cell_result: Callable[[_ActCellResult], Awaitable[None]] | None = None,
    on_done: Callable[[], Awaitable[None]] | None = None,
) -> Any:
    """Run host-supplied cells in the serving REPL's live namespace."""
    from . import repl

    state = _ActState()
    token = _active_state.set(state)
    cancel_task = asyncio.ensure_future(cancel) if cancel is not None else None
    try:
        iterator = cells.__aiter__()
        while True:
            request_task = asyncio.ensure_future(anext(iterator))
            try:
                if cancel_task is not None:
                    completed, _ = await asyncio.wait(
                        (request_task, cancel_task), return_when=asyncio.FIRST_COMPLETED
                    )
                    if cancel_task in completed:
                        cancel_task.result()
                        request_task.cancel()
                        await asyncio.gather(request_task, return_exceptions=True)
                        raise _ActInterrupted
                try:
                    cell = await request_task
                except StopAsyncIteration:
                    break
            finally:
                if not request_task.done():
                    request_task.cancel()
                    await asyncio.gather(request_task, return_exceptions=True)

            started_at = time.monotonic()
            cell_task = asyncio.create_task(repl._run_shared_cell(cell))
            state.cell_task = cell_task
            try:
                try:
                    if cancel_task is not None:
                        completed, _ = await asyncio.wait(
                            (cell_task, cancel_task), return_when=asyncio.FIRST_COMPLETED
                        )
                        if cancel_task in completed:
                            cancel_task.result()
                            cell_task.cancel()
                            await asyncio.gather(cell_task, return_exceptions=True)
                            raise _ActInterrupted
                    value, has_trailing, stdout, stderr, error = await cell_task
                except (asyncio.CancelledError, KeyboardInterrupt) as exc:
                    raise _ActInterrupted from exc
                except BaseException as exc:  # compile failures are ordinary cell results
                    value, has_trailing, stdout, stderr, error = None, False, "", "", exc

                if isinstance(error, _ActDone):
                    if on_done is not None:
                        await on_done()
                    return state.value
                if isinstance(error, (asyncio.CancelledError, KeyboardInterrupt)):
                    raise _ActInterrupted from error

                result_text: str | None = None
                if error is None and has_trailing and value is not None:
                    try:
                        result_text = repr(value)
                    except BaseException as exc:  # a broken repr is a cell failure
                        error = exc
                if on_cell_result is not None:
                    await on_cell_result(
                        _ActCellResult(
                            duration_ms=(time.monotonic() - started_at) * 1000,
                            stdout=stdout[:65536],
                            stderr=stderr[:65536],
                            result=result_text[:65536] if result_text is not None else None,
                            error=_format_error(error)[:65536] if error is not None else None,
                        )
                    )
            finally:
                if not cell_task.done():
                    cell_task.cancel()
                    await asyncio.gather(cell_task, return_exceptions=True)
                state.cell_task = None
        raise RuntimeError("Act ended without calling rlm.done()")
    except asyncio.CancelledError as exc:
        raise _ActInterrupted from exc
    finally:
        if cancel_task is not None and not cancel_task.done():
            cancel_task.cancel()
            await asyncio.gather(cancel_task, return_exceptions=True)
        _active_state.reset(token)
