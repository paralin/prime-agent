from __future__ import annotations

import asyncio
import importlib.util
import sys
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from rlm import bash

SKILL = Path(__file__).parents[2] / "packages/coding-agent/skills/external-event/src/external_event/__init__.py"


def _load_skill():
    spec = importlib.util.spec_from_file_location("external_event_test", SKILL)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class ExternalEventSkillTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.module = _load_skill()

    async def test_emit_builds_typed_host_request(self) -> None:
        host = AsyncMock(return_value={"deliveryStatus": "queued"})
        with patch.object(self.module, "host_request", host):
            receipt = await self.module.emit("watch", "event-1", "done")
        self.assertEqual(receipt, {"deliveryStatus": "queued"})
        host.assert_awaited_once_with(
            "session.external_event.emit",
            {
                "name": "watch",
                "event_id": "event-1",
                "text": "done",
                "delivery_policy": "followUp",
            },
        )

    async def test_watch_bash_emits_once_and_retains_result(self) -> None:
        emitted = asyncio.Event()
        payloads = []

        async def host(_request_type, payload):
            payloads.append(payload)
            emitted.set()
            return {"deliveryStatus": "delivered"}

        with patch.object(self.module, "host_request", host):
            job_id = self.module.watch_bash(bash("printf 'first\nsecond\n'"), "capture", tail_lines=1)
            await asyncio.wait_for(emitted.wait(), timeout=5)

        self.assertEqual(len(payloads), 1)
        self.assertEqual(payloads[0]["event_id"], job_id)
        self.assertIn("Exit code: 0", payloads[0]["text"])
        output_tail = payloads[0]["text"].split("Output tail:\n", 1)[1]
        self.assertEqual(output_tail, "second")
        info = self.module.get_job(job_id)
        self.assertEqual(info.status, "completed")
        self.assertEqual(info.exit_code, 0)
        self.assertEqual(self.module.list_jobs(), [info])

    async def test_timed_out_bash_still_emits_terminal_result(self) -> None:
        host = AsyncMock(return_value={"deliveryStatus": "delivered"})
        with patch.object(self.module, "host_request", host):
            job_id = self.module.watch_bash(bash("sleep 30", timeout=0.05), "bounded")
            assert self.module._jobs[job_id].task is not None
            await asyncio.wait_for(self.module._jobs[job_id].task, timeout=5)
        info = self.module.get_job(job_id)
        self.assertEqual(info.status, "timed_out")
        self.assertNotEqual(info.exit_code, 0)
        self.assertIn("timed_out", host.await_args.args[1]["text"])
        self.assertIn("TimeoutError", host.await_args.args[1]["text"])

    async def test_rejects_duplicate_watch_registration(self) -> None:
        host = AsyncMock(return_value={"deliveryStatus": "delivered"})
        with patch.object(self.module, "host_request", host):
            job = bash("printf done")
            job_id = self.module.watch_bash(job, "first")
            with self.assertRaisesRegex(ValueError, f"already watched as {job_id}"):
                self.module.watch_bash(job, "second")
            assert self.module._jobs[job_id].task is not None
            await self.module._jobs[job_id].task
        host.assert_awaited_once()

    async def test_cancel_job_kills_local_group_and_emits(self) -> None:
        emitted = asyncio.Event()

        async def host(_request_type, _payload):
            emitted.set()
            return {"deliveryStatus": "queued"}

        with patch.object(self.module, "host_request", host):
            job_id = self.module.watch_bash(bash("sleep 30"), "long job")
            info = await asyncio.wait_for(self.module.cancel_job(job_id), timeout=5)
            await asyncio.wait_for(emitted.wait(), timeout=5)

        self.assertEqual(info.status, "cancelled")
        self.assertNotEqual(info.exit_code, 0)

    async def test_rejects_invalid_registration_before_starting_watcher(self) -> None:
        job = bash("printf ok")
        with self.assertRaises(ValueError):
            self.module.watch_bash(job, " ")
        await job
        self.assertEqual(self.module.list_jobs(), [])


if __name__ == "__main__":
    unittest.main()
