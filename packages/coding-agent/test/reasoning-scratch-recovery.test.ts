import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fauxAssistantMessage, registerFauxProvider } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
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
	it.each(["success", "closeout fails", "exhausts again"])("handles %s without repeated recovery", async (outcome) => {
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
				noTools: "all",
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
			faux.setResponses([
				() => {
					calls++;
					return exhausted();
				},
				(context) => {
					calls++;
					expect(context.messages.some((m) => m.role === "user")).toBe(true);
					if (outcome === "closeout fails") return exhausted();
					mkdirSync(dirname(checkpoint), { recursive: true });
					writeFileSync(checkpoint, "* TODO Active request\nContinue the requested boundary check.\n");
					return fauxAssistantMessage("Checkpoint written");
				},
				() => {
					calls++;
					if (outcome === "exhausts again") return exhausted();
					return fauxAssistantMessage("Boundary check complete");
				},
				() => {
					calls++;
					throw new Error("Unexpected fourth request");
				},
			]);
			await session.prompt("Run the boundary check");
			await session.waitForIdle();
			if (outcome !== "closeout fails") await vi.waitFor(() => expect(calls).toBe(3));
			await session.waitForIdle();
			const branch = session.sessionManager.getBranch();
			expect(calls).toBe(outcome === "closeout fails" ? 2 : 3);
			expect(branch.filter((e) => e.type === "compaction")).toHaveLength(outcome === "closeout fails" ? 0 : 1);
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
