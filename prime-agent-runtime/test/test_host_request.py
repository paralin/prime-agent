from __future__ import annotations

import asyncio
import unittest
from unittest.mock import patch

import rlm
from rlm import repl


class FakeExchange:
    instances: list["FakeExchange"] = []

    def __init__(self, payload: dict[str, object]) -> None:
        self.payload = payload
        self.closed = False
        self.instances.append(self)

    async def receive(self) -> dict[str, object]:
        await asyncio.Event().wait()
        raise AssertionError("unreachable")

    def close(self) -> None:
        self.closed = True


class HostRequestTest(unittest.IsolatedAsyncioTestCase):
    async def test_cancelled_request_always_closes_exchange(self) -> None:
        FakeExchange.instances.clear()
        with patch.object(repl, "open_host_exchange", FakeExchange):
            task = asyncio.create_task(rlm.host_request("agent_message.wait", {"timeout_ms": 1000}))
            await asyncio.sleep(0)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assertEqual(FakeExchange.instances[0].payload, {
            "timeout_ms": 1000,
            "type": "agent_message.wait",
        })
        self.assertTrue(FakeExchange.instances[0].closed)


if __name__ == "__main__":
    unittest.main()
