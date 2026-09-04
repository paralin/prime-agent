import type { AssistantMessage } from "@earendil-works/pi-ai";
import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { expect, it, vi } from "vitest";
import { createHarness } from "../harness.js";

it.each(["stop", "error", "aborted"] as const)(
	"uses the displayed local Merge estimate for threshold checks after %s",
	async (stopReason) => {
		const harness = await createHarness({
			settings: { compaction: { enabled: true, triggerContextTokens: 1000 } },
		});
		try {
			const successful = {
				...fauxAssistantMessage("previous answer"),
				provider: "merge-gateway",
				usage: {
					input: 10,
					output: 10,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 20,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
			};
			const last = { ...successful, stopReason };
			harness.session.agent.state.messages = [
				{ role: "user", content: "task context ".repeat(1000), timestamp: Date.now() },
				successful,
				last,
			];
			const internals = harness.session as unknown as {
				_checkCompaction(message: AssistantMessage, skipAbortedCheck: boolean): Promise<boolean>;
				_runAutoCompaction(reason: string, retry: boolean): Promise<boolean>;
			};
			const compact = vi.spyOn(internals, "_runAutoCompaction").mockResolvedValue(false);
			expect(harness.session.getContextUsage()?.tokens).toBeGreaterThan(1000);
			await internals._checkCompaction(last, false);
			expect(compact).toHaveBeenCalledWith("threshold", false);
		} finally {
			vi.restoreAllMocks();
			harness.cleanup();
		}
	},
);
