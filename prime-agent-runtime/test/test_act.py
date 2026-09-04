from __future__ import annotations

import asyncio
import unittest
from collections.abc import AsyncIterator

import rlm
from rlm import repl
from rlm._act import _ActInterrupted, _run_cells, done


async def cells(*sources: str) -> AsyncIterator[str]:
    for source in sources:
        yield source


class ActApiTest(unittest.IsolatedAsyncioTestCase):
    async def test_validates_prompt_before_opening_an_exchange(self) -> None:
        with self.assertRaisesRegex(TypeError, "prompt must be str"):
            await rlm.act(1)  # type: ignore[arg-type]
        with self.assertRaisesRegex(ValueError, "prompt must not be empty"):
            await rlm.act("  ")
        with self.assertRaisesRegex(TypeError, "model must be str or None"):
            await rlm.act("task", model=1)  # type: ignore[arg-type]
        with self.assertRaisesRegex(ValueError, "model must not be empty"):
            await rlm.rlm.act("task", model="  ")

    async def test_publishes_the_host_platform_cancellation_capability(self) -> None:
        self.assertIn(
            rlm.ACT_CANCELLATION_CAPABILITY, {"posix-managed", "cooperative-only"}
        )
        self.assertEqual(
            rlm.rlm.ACT_CANCELLATION_CAPABILITY, rlm.ACT_CANCELLATION_CAPABILITY
        )
        self.assertIn("ACT_CANCELLATION_CAPABILITY", rlm.act.__doc__ or "")


class ActCellRunnerTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.previous_namespace = repl._user_namespace
        repl._user_namespace = {
            "__builtins__": __builtins__,
            "asyncio": asyncio,
            "done": done,
            "rlm": rlm.rlm,
        }

    def tearDown(self) -> None:
        repl._user_namespace = self.previous_namespace

    @property
    def namespace(self) -> dict[str, object]:
        assert repl._user_namespace is not None
        return repl._user_namespace

    async def test_returns_exact_object_and_stops_terminal_cell(self) -> None:
        returned = await _run_cells(
            cells("original = object()", "done(original)\nafter_done = True")
        )

        self.assertIs(returned, self.namespace["original"])
        self.assertNotIn("after_done", self.namespace)

    async def test_nested_done_returns_exact_identity_and_restores_calling_cell(
        self,
    ) -> None:
        nested_source = object()
        self.namespace["nested_source"] = nested_source

        async def nested_act() -> object:
            return await _run_cells(cells("done(nested_source)"))

        self.namespace["nested_act"] = nested_act
        outer_source = object()
        self.namespace["outer_source"] = outer_source
        returned = await _run_cells(
            cells(
                "nested_result = await nested_act()\n"
                "nested_identity = nested_result is nested_source\n"
                "continued_after_nested = True",
                "done(outer_source)",
            )
        )

        self.assertIs(self.namespace["nested_result"], nested_source)
        self.assertIs(self.namespace["nested_identity"], True)
        self.assertIs(self.namespace["continued_after_nested"], True)
        self.assertIs(returned, outer_source)

    async def test_reports_result_and_error_before_the_next_cell(self) -> None:
        results: list[object] = []

        async def record(result: object) -> None:
            results.append(result)

        returned = await _run_cells(
            cells("6 * 7", "raise ValueError('expected')", "done('finished')"),
            on_cell_result=record,
        )

        self.assertEqual(returned, "finished")
        self.assertEqual(getattr(results[0], "result"), "42")
        self.assertIn("ValueError: expected", getattr(results[1], "error"))

    async def test_none_is_a_valid_exact_result(self) -> None:
        self.assertIsNone(await _run_cells(cells("done(None)")))

    async def test_cancellation_stops_an_awaited_cell(self) -> None:
        cancelled = asyncio.Event()

        async def wait_for_cancel() -> None:
            await cancelled.wait()

        task = asyncio.create_task(
            _run_cells(
                cells("started = True\nawait asyncio.sleep(60)"),
                cancel=wait_for_cancel(),
            )
        )
        while "started" not in self.namespace:
            await asyncio.sleep(0)
        cancelled.set()

        with self.assertRaises(_ActInterrupted):
            await task

    async def test_exhaustion_requires_done(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "ended without calling rlm.done"):
            await _run_cells(cells("ordinary_value = 1"))
        with self.assertRaisesRegex(RuntimeError, "only valid in the active Act cell"):
            done(None)

    async def test_rejects_done_from_a_detached_inner_task(self) -> None:
        returned = await _run_cells(
            cells(
                "gate = asyncio.Event()\n"
                "async def late_done():\n"
                "    await gate.wait()\n"
                "    done('detached')\n"
                "late_task = asyncio.create_task(late_done())",
                "gate.set()\n"
                "try:\n"
                "    await late_task\n"
                "except RuntimeError:\n"
                "    detached_done_rejected = True\n"
                "done('active-cell')",
            )
        )

        self.assertEqual(returned, "active-cell")
        self.assertIs(self.namespace["detached_done_rejected"], True)


if __name__ == "__main__":
    unittest.main()
