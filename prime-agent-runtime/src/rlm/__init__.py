"""Tiny rlm-compatible kernel shim for Prime Agent."""

from __future__ import annotations

import asyncio
import os
import sys
import types
from dataclasses import dataclass
from pathlib import Path
from typing import Any, AsyncIterator

from ._act import _ActCellResult, _ActInterrupted, _run_cells, done
from .bash import BashHandle, BashResult, bash, rg, rsync, ssh_forward
from .harness import HarnessEntry, HarnessScope, HarnessState, RefinementEvent, get_harness_state
ACT_CANCELLATION_CAPABILITY = "posix-managed" if os.name == "posix" else "cooperative-only"
RLM_SERVICE_TIERS = ("auto", "default", "flex", "scale", "priority")


def _validate_service_tier(value: Any) -> None:
    if value is None or value in RLM_SERVICE_TIERS:
        return
    raise ValueError(f"service_tier must be one of {', '.join(RLM_SERVICE_TIERS)} or None")


class ActError(RuntimeError):
    """An Act ended without an accepted terminal value."""


class ActCancelledError(ActError):
    """The host accepted Act cancellation under ACT_CANCELLATION_CAPABILITY."""



@dataclass(frozen=True)
class RLMSpawnHandle:
    rlm_child_id: str
    name: str
    session_dir: Path
    model: str


@dataclass(frozen=True)
class RLMModel:
    provider: str
    id: str
    name: str
    selector: str
    concrete_selector: str | None = None
    available: bool | None = None


@dataclass(frozen=True)
class RLMSubagent:
    rlm_child_id: str
    active_session_id: str | None
    session_id: str | None
    session_name: str
    session_dir: Path
    status: str


def _spawn_handle_from_payload(payload: Any) -> RLMSpawnHandle:
    if not isinstance(payload, dict):
        raise RuntimeError("rlm.run returned an invalid spawn handle")
    child_id = payload.get("rlm_child_id")
    name = payload.get("name")
    session_dir = payload.get("session_dir")
    model = payload.get("model")
    if not all(isinstance(value, str) and value for value in (child_id, name, session_dir, model)):
        raise RuntimeError("rlm.run returned an invalid spawn handle")
    return RLMSpawnHandle(
        rlm_child_id=child_id,
        name=name,
        session_dir=Path(session_dir),
        model=model,
    )


class _ActExchange:
    def __init__(self, prompt: str, model: str | None = None) -> None:
        from . import repl

        payload: dict[str, str] = {"prompt": prompt, "type": "rlm.act"}
        if model is not None:
            payload["model"] = model
        self._exchange = repl.open_host_exchange(payload)
        self._messages: asyncio.Queue[dict[str, Any] | BaseException] = asyncio.Queue()
        self._cancelled = asyncio.Event()
        self._closed = False
        self._reader = asyncio.create_task(self._read_messages())

    async def _read_messages(self) -> None:
        try:
            while True:
                message = await self._exchange.receive()
                result = message.get("result")
                if message.get("status") == "ok" and isinstance(result, dict):
                    message = {"status": "ok", **result}
                if message.get("status") == "aborted" or (
                    message.get("status") == "ok" and message.get("outcome") == "cancelled"
                ):
                    self._cancelled.set()
                self._messages.put_nowait(message)
        except BaseException as error:
            self._cancelled.set()
            self._messages.put_nowait(error)

    async def _receive(self) -> dict[str, Any]:
        message = await self._messages.get()
        if isinstance(message, BaseException):
            raise message
        return message

    async def wait_cancelled(self) -> None:
        await self._cancelled.wait()

    async def cells(self) -> AsyncIterator[str]:
        while True:
            message = await self._receive()
            status = message.get("status")
            if status == "event" and message.get("type") == "cell":
                code = message.get("code")
                if not isinstance(code, str):
                    raise ActError("Act returned a cell event without string code")
                yield code
                continue
            if status == "ok":
                if message.get("outcome") == "cancelled":
                    self._cancelled.set()
                    raise _ActInterrupted
                return
            if status == "error":
                raise ActError(str(message.get("error") or "Act host failed"))
            if status == "aborted":
                self._cancelled.set()
                raise _ActInterrupted
            raise ActError(f"Act returned an unexpected message: {message!r}")

    async def send_cell_result(self, result: _ActCellResult) -> None:
        self._exchange.send(
            {
                "type": "cell_result",
                "durationMs": result.duration_ms,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "result": result.result,
                "error": result.error,
            }
        )

    async def acknowledge_done(self) -> None:
        self._exchange.send({"type": "done"})
        message = await self._receive()
        status = message.get("status")
        if status == "ok" and message.get("outcome") == "done":
            return
        if status == "ok" and message.get("outcome") == "cancelled":
            raise _ActInterrupted
        if status == "error":
            raise ActError(str(message.get("error") or "Act completion failed"))
        if status == "aborted":
            raise _ActInterrupted
        raise ActError(f"Act completion returned an unexpected message: {message!r}")

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._reader.cancel()
        self._exchange.close()


async def act(prompt: str, model: str | None = None) -> Any:
    """Run one retained low-level task in the serving REPL's live namespace.

    Cancellation always aborts provider work and cooperative awaited Python.
    ``ACT_CANCELLATION_CAPABILITY == "posix-managed"`` additionally covers a
    correlated synchronous inner-cell interrupt and managed ``bash()`` process
    groups. ``"cooperative-only"`` does not promise to stop synchronous Python
    or blocking shell work before it returns.
    """
    if not isinstance(prompt, str):
        raise TypeError(f"prompt must be str, got {type(prompt).__name__}")
    if not prompt.strip():
        raise ValueError("prompt must not be empty")
    if model is not None and not isinstance(model, str):
        raise TypeError(f"model must be str or None, got {type(model).__name__}")
    if isinstance(model, str):
        model = model.strip()
        if not model:
            raise ValueError("model must not be empty")
    exchange = _ActExchange(prompt, model)
    try:
        return await _run_cells(
            exchange.cells(),
            cancel=exchange.wait_cancelled(),
            on_cell_result=exchange.send_cell_result,
            on_done=exchange.acknowledge_done,
        )
    except _ActInterrupted as error:
        raise ActCancelledError("Act was cancelled") from error
    except ActError:
        raise
    except RuntimeError as error:
        raise ActError(str(error)) from error
    finally:
        exchange.close()


def _parse_host_reply(request_type: str, reply: dict[str, Any]) -> dict[str, Any]:
    status = reply.get("status")
    if status == "ok":
        return reply["result"]
    if status == "error":
        raise RuntimeError(str(reply.get("error") or f"host request {request_type} failed"))
    raise RuntimeError(f"host request {request_type} returned unexpected status: {status!r}")


async def host_request(request_type: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
    """Send a typed request to the Prime Agent host and await its reply.

    This is the kernel side of the generic host bridge: Python skills call
    ``await host_request("<type>", {...})`` and the TypeScript host dispatches
    on the type. Raises RuntimeError when the host reports an error or when no
    handler for the type is registered in this session.
    """
    if not isinstance(request_type, str) or not request_type:
        raise TypeError("request_type must be a non-empty str")
    if payload is not None and not isinstance(payload, dict):
        raise TypeError(f"payload must be a dict or None, got {type(payload).__name__}")
    from . import repl

    # request_type goes last so a payload "type" key cannot reroute the request.
    reply = await repl.host_request({**(payload or {}), "type": request_type})
    return _parse_host_reply(request_type, reply)


def emit(data: dict[str, Any]) -> None:
    """Ship one display event to the host."""
    from . import repl

    repl.emit(data)


async def run(prompt: str, **kwargs: Any) -> RLMSpawnHandle:
    """Spawn a recursive Prime Agent child and return once its task is admitted.

    ``model`` selects a child with an exact ``provider/model`` selector.
    ``service_tier`` requests an allowed override of the parent tier for this child.
    """
    if not isinstance(prompt, str):
        raise TypeError(f"prompt must be str, got {type(prompt).__name__}")
    if "service_tier" in kwargs:
        _validate_service_tier(kwargs["service_tier"])
    payload = await host_request("rlm.run", {"prompt": prompt, "kwargs": kwargs})
    return _spawn_handle_from_payload(payload)


def _model_from_payload(payload: Any) -> RLMModel:
    if not isinstance(payload, dict):
        raise RuntimeError("rlm.find_models returned an invalid model entry")
    provider = payload.get("provider")
    model_id = payload.get("id")
    name = payload.get("name")
    selector = payload.get("selector")
    if not all(isinstance(value, str) and value for value in (provider, model_id, name, selector)):
        raise RuntimeError("rlm.find_models returned an invalid model entry")
    concrete_selector = payload.get("concreteSelector")
    if concrete_selector is not None and (not isinstance(concrete_selector, str) or not concrete_selector):
        raise RuntimeError("rlm.find_models returned an invalid model entry")
    available = payload.get("available")
    if available is not None and not isinstance(available, bool):
        raise RuntimeError("rlm.find_models returned an invalid model entry")
    return RLMModel(
        provider=provider,
        id=model_id,
        name=name,
        selector=selector,
        concrete_selector=concrete_selector,
        available=available,
    )


async def find_models(query: str = "", limit: int = 8) -> list[RLMModel]:
    """Search a bounded list of models backed by active user credentials."""
    if not isinstance(query, str):
        raise TypeError(f"query must be str, got {type(query).__name__}")
    if not isinstance(limit, int):
        raise TypeError(f"limit must be int, got {type(limit).__name__}")
    payload = await host_request("rlm.find_models", {"query": query, "limit": limit})
    models = payload.get("models")
    if not isinstance(models, list):
        raise RuntimeError("rlm.find_models returned an invalid models list")
    return [_model_from_payload(model) for model in models]


def _subagent_from_payload(payload: Any, operation: str = "rlm.list_subagents") -> RLMSubagent:
    if not isinstance(payload, dict):
        raise RuntimeError(f"{operation} returned an invalid subagent entry")
    child_id = payload.get("rlm_child_id")
    active_session_id = payload.get("active_session_id")
    session_id = payload.get("session_id")
    session_name = payload.get("session_name")
    session_dir = payload.get("session_dir")
    status = payload.get("status")
    if not isinstance(child_id, str) or not child_id:
        raise RuntimeError(f"{operation} entry is missing rlm_child_id")
    if active_session_id is not None and not isinstance(active_session_id, str):
        raise RuntimeError(f"{operation} entry has invalid active_session_id")
    if session_id is not None and not isinstance(session_id, str):
        raise RuntimeError(f"{operation} entry has invalid session_id")
    if not isinstance(session_name, str) or not session_name:
        raise RuntimeError(f"{operation} entry is missing session_name")
    if not isinstance(session_dir, str) or not session_dir:
        raise RuntimeError(f"{operation} entry is missing session_dir")
    if status not in {"running", "completed", "error"}:
        raise RuntimeError(f"{operation} entry has invalid status")
    return RLMSubagent(
        rlm_child_id=child_id,
        active_session_id=active_session_id,
        session_id=session_id,
        session_name=session_name,
        session_dir=Path(session_dir),
        status=status,
    )


async def list_subagents() -> list[RLMSubagent]:
    """List direct RLM children retained by the current parent session."""
    payload = await host_request("rlm.list_subagents")
    entries = payload.get("subagents")
    if not isinstance(entries, list):
        raise RuntimeError("rlm.list_subagents returned an invalid subagents registry")
    return [_subagent_from_payload(entry) for entry in entries]


async def delete_subagent(target: str | RLMSubagent) -> RLMSubagent:
    """Delete one running or retained direct child from the current parent session."""
    if isinstance(target, RLMSubagent):
        selector = target.rlm_child_id
    elif isinstance(target, str):
        selector = target.strip()
        if not selector:
            raise ValueError("target must not be empty")
    else:
        raise TypeError(f"target must be str or RLMSubagent, got {type(target).__name__}")
    payload = await host_request("rlm.delete_subagent", {"target": selector})
    return _subagent_from_payload(payload.get("subagent"), "rlm.delete_subagent")


class _HarnessProxy:
    """Resolve the harness state against the current environment on every access.

    Session env vars may be applied after import, so a state bound at import
    time could freeze an env-less resolution. Resolving per access picks up the env
    applied after fork. Resolution must never raise (a failure inside the kernel
    namespace would take down the kernel). When the local store is genuinely
    unconfigured (no session env, e.g. --no-session) reads see an empty view but
    local writes raise instructively instead of vanishing on kernel exit; any
    other resolution failure degrades to a shared in-memory store until local
    resolution starts succeeding.
    """

    _fallback: HarnessState | None = None
    _unpersisted: HarnessState | None = None

    def _resolve(self) -> HarnessState:
        try:
            return get_harness_state()
        except RuntimeError as exc:
            if "Local harness state requires" in str(exc):
                if _HarnessProxy._unpersisted is None:
                    _HarnessProxy._unpersisted = HarnessState(
                        in_memory=True,
                        local_write_error=(
                            f"{exc} This session has no persistent local harness store; "
                            "pass global_=True to persist across sessions."
                        ),
                    )
                return _HarnessProxy._unpersisted
            return self._degraded()
        except Exception:  # pragma: no cover - harness access must never raise
            return self._degraded()

    @staticmethod
    def _degraded() -> HarnessState:
        if _HarnessProxy._fallback is None:
            _HarnessProxy._fallback = HarnessState(in_memory=True)
        return _HarnessProxy._fallback

    def __getattr__(self, name: str) -> Any:
        return getattr(self._resolve(), name)

    def __repr__(self) -> str:
        return repr(self._resolve())


_harness_state = _HarnessProxy()


class _RLMCallable:
    ACT_CANCELLATION_CAPABILITY = ACT_CANCELLATION_CAPABILITY
    harness = _harness_state
    get_harness_state = staticmethod(get_harness_state)
    done = staticmethod(done)

    async def act(self, prompt: str, model: str | None = None) -> Any:
        return await act(prompt, model)

    async def run(self, prompt: str, **kwargs: Any) -> RLMSpawnHandle:
        return await run(prompt, **kwargs)

    async def find_models(self, query: str = "", limit: int = 8) -> list[RLMModel]:
        return await find_models(query, limit)

    async def list_subagents(self) -> list[RLMSubagent]:
        return await list_subagents()

    async def delete_subagent(self, target: str | RLMSubagent) -> RLMSubagent:
        return await delete_subagent(target)

    async def __call__(self, prompt: str, **kwargs: Any) -> RLMSpawnHandle:
        return await run(prompt, **kwargs)


rlm = _RLMCallable()
harness = _harness_state


class _CallableModule(types.ModuleType):
    async def __call__(self, prompt: str, **kwargs: Any) -> RLMSpawnHandle:
        return await run(prompt, **kwargs)


sys.modules[__name__].__class__ = _CallableModule

__all__ = [
    "ACT_CANCELLATION_CAPABILITY",
    "ActCancelledError",
    "ActError",
    "BashHandle",
    "BashResult",
    "HarnessEntry",
    "HarnessScope",
    "HarnessState",
    "McpIntegration",
    "McpToolError",
    "NotEnabled",
    "RLMModel",
    "RLMSpawnHandle",
    "RLMSubagent",
    "RefinementEvent",
    "act",
    "bash",
    "rg",
    "rsync",
    "ssh_forward",
    "delete_subagent",
    "done",
    "emit",
    "find_models",
    "get_harness_state",
    "harness",
    "host_request",
    "list_subagents",
    "rlm",
    "run",
]

# Lazily re-export the MCP base class. Kept lazy so `import rlm` never requires
# the optional `mcp` SDK — only integration packages that subclass it do.
_LAZY_MCP = {"McpIntegration", "McpToolError", "NotEnabled"}


def __getattr__(name: str) -> Any:  # noqa: D401 - module-level lazy attr hook
    if name in _LAZY_MCP:
        from . import mcp_base

        return getattr(mcp_base, name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
