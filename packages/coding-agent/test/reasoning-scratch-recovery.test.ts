import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fauxAssistantMessage, registerFauxProvider } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { resolveScratchHandoffPath } from "../src/core/compaction/scratch-handoff.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { createAgentSession } from "../src/core/sdk.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { createTestResourceLoader, createTestSessionManager } from "./utilities.js";

function exhausted() {
	const message = fauxAssistantMessage([{ type: "thinking", thinking: "failed deliberation must not replay" }], {
		stopReason: "length",
	});
	message.diagnostics = [
		{ type: "provider_warning", timestamp: 0, error: { code: "reasoning_exhausted", message: "No answer" } },
	];
	return message;
}

describe("bounded reasoning scratch recovery", () => {
	it.each([
		{ outcome: "success", expectedCalls: 3, expectedCompactions: 1 },
		{ outcome: "closeout fails", expectedCalls: 2, expectedCompactions: 0 },
		{ outcome: "exhausts again", expectedCalls: 3, expectedCompactions: 1 },
		{ outcome: "exhausts after progress", expectedCalls: 6, expectedCompactions: 2 },
		{ outcome: "exhausts with queued child", expectedCalls: 6, expectedCompactions: 2 },
		{ outcome: "answer then child message", expectedCalls: 6, expectedCompactions: 2 },
		{ outcome: "notification without progress", expectedCalls: 4, expectedCompactions: 1 },
		{ outcome: "tool fails", expectedCalls: 4, expectedCompactions: 1 },
	])("handles $outcome with bounded recovery", async ({ outcome, expectedCalls, expectedCompactions }) => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-reasoning-recovery-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff", triggerContextTokens: 200000 },
				retry: { enabled: false },
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				cwd: tempDir,
				tools: ["check_boundary"],
				includeGoals: false,
				customTools: [
					{
						name: "check_boundary",
						label: "Check boundary",
						description: "Check the active task boundary.",
						parameters: Type.Object({}),
						execute: async () => {
							if (outcome === "tool fails") throw new Error("Boundary check failed");
							return { content: [{ type: "text", text: "Boundary verified" }], details: {} };
						},
					},
				],
				sessionManager: createTestSessionManager(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
			});
			const checkpoint = resolveScratchHandoffPath({
				cwd: tempDir,
				rootDir,
				sessionId: session.sessionId,
			}).absolutePath;
			let calls = 0;
			const checksBoundary = ["exhausts after progress", "exhausts with queued child", "tool fails"].includes(
				outcome,
			);
			const writeCheckpoint = () => {
				mkdirSync(dirname(checkpoint), { recursive: true });
				writeFileSync(checkpoint, "* TODO Active request\nContinue the requested boundary check.\n");
				return fauxAssistantMessage("Checkpoint written");
			};
			faux.setResponses([
				() => {
					calls++;
					return exhausted();
				},
				(context) => {
					calls++;
					expect(context.messages.some((m) => m.role === "user")).toBe(true);
					if (outcome === "closeout fails") return exhausted();
					return writeCheckpoint();
				},
				async () => {
					calls++;
					if (outcome === "exhausts again") return exhausted();
					if (outcome === "notification without progress") {
						await session.sendCustomMessage(
							{ customType: "agent_message", content: "Child boundary verification completed", display: true },
							{ triggerTurn: true, deliverAs: "steer" },
						);
						return exhausted();
					}
					if (checksBoundary)
						return fauxAssistantMessage(
							[{ type: "toolCall", id: "boundary-check", name: "check_boundary", arguments: {} }],
							{ stopReason: "toolUse" },
						);
					return fauxAssistantMessage("Boundary check complete");
				},
				async () => {
					calls++;
					if (outcome === "exhausts with queued child") {
						await session.sendCustomMessage(
							{ customType: "agent_message", content: "Child boundary verification completed", display: true },
							{ triggerTurn: true, deliverAs: "steer" },
						);
					}
					if (
						checksBoundary ||
						outcome === "answer then child message" ||
						outcome === "notification without progress"
					)
						return exhausted();
					throw new Error("Unexpected fourth request");
				},
				() => {
					calls++;
					return writeCheckpoint();
				},
				(context) => {
					calls++;
					if (outcome === "exhausts with queued child") {
						expect(JSON.stringify(context.messages)).toContain("Child boundary verification completed");
					}
					return fauxAssistantMessage("Task complete after second recovery");
				},
			]);
			await session.prompt("Run the boundary check");
			await session.waitForHeadlessIdle();
			if (outcome === "answer then child message") {
				await session.sendCustomMessage(
					{ customType: "agent_message", content: "Check the child result", display: true },
					{ triggerTurn: true },
				);
				await session.waitForHeadlessIdle();
			}
			const branch = session.sessionManager.getBranch();
			expect(calls).toBe(expectedCalls);
			expect(branch.filter((e) => e.type === "compaction")).toHaveLength(expectedCompactions);
			expect(session.isStreaming).toBe(false);
			expect(session.isCompacting).toBe(false);
			expect(session.isRetrying).toBe(false);
			expect(session.hasPendingSessionWork).toBe(false);
			if (outcome === "exhausts with queued child" || outcome === "notification without progress") {
				expect(branch.filter((e) => e.type === "custom_message" && e.customType === "agent_message")).toHaveLength(
					1,
				);
			}
			expect(JSON.stringify(branch.filter((e) => e.type === "compaction"))).not.toContain(
				"failed deliberation must not replay",
			);
			if (outcome === "success") {
				const last = branch.filter((e) => e.type === "message" && e.message.role === "assistant").at(-1);
				if (last?.type !== "message" || last.message.role !== "assistant")
					throw new Error("Missing resumed response");
				const usage = last.message.usage;
				expect(last.message.diagnostics).toEqual(
					expect.arrayContaining([
						expect.objectContaining({
							type: "agent_request_metrics",
							details: expect.objectContaining({
								phase: "recovery",
								cacheReadRatio: usage.cacheRead / (usage.input + usage.cacheRead + usage.cacheWrite),
							}),
						}),
					]),
				);
			}
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});
});
