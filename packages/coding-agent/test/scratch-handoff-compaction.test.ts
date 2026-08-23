import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fauxAssistantMessage, registerFauxProvider } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { SCRATCH_HANDOFF_READ_CUSTOM_TYPE } from "../src/core/compaction/scratch-handoff.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { createAgentSession } from "../src/core/sdk.js";
import { SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { createTestResourceLoader } from "./utilities.js";

describe("scratch handoff compaction", () => {
	it("injects continuity instructions and rebuilds around the checkpoint without an LLM summary", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-handoff-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			let providerCalls = 0;
			let providerSystemPrompt = "";
			faux.setResponses([
				(context) => {
					providerCalls++;
					providerSystemPrompt = context.systemPrompt ?? "";
					return fauxAssistantMessage("work done");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "native-or-scratch" },
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: SessionManager.create(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
				noTools: "all",
				cwd: tempDir,
			});

			await session.prompt("do some work");
			await session.waitForIdle();
			expect(providerCalls).toBe(1);
			expect(providerSystemPrompt).toContain("Scratch continuity:");
			expect(providerSystemPrompt).toContain(rootDir);

			// No checkpoint yet: the requested compaction still routes to the
			// scratch rebuild and carries the recent conversation as the delta.
			const result = await session.compact();
			expect(result.summary).toContain("Scratch handoff");

			const entries = session.sessionManager.getBranch();
			const readMarker = [...entries]
				.reverse()
				.find((e) => e.type === "custom_message" && e.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE);
			expect(readMarker).toBeDefined();
			const compaction = entries.find((e) => e.type === "compaction" && e.firstKeptEntryId === readMarker!.id);
			expect(compaction).toBeDefined();

			// The rebuilt context starts at the resume payload; pre-compaction
			// turns are gone and no summarization request was made.
			const resumedMessages = session.agent.state.messages;
			expect(JSON.stringify(resumedMessages)).toContain("No scratch checkpoint exists yet");
			// The recent turn rides inside the resume payload as the bounded delta.
			expect(resumedMessages.filter((m) => m.role === "user" || m.role === "assistant").length).toBe(0);
			expect(providerCalls).toBe(1);

			// The persisted read marker pins the checkpoint path for later sessions.
			const details = (readMarker as { details?: { path?: string } }).details;
			expect(details?.path).toContain(rootDir);

			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("records a closeout write marker when the staged turn changed the document", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-closeout-"));
		const faux = registerFauxProvider();
		try {
			faux.setResponses([fauxAssistantMessage("ok")]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const rootDir = join(tempDir, "scratch");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "native-or-scratch" },
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: SessionManager.create(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
				noTools: "all",
				cwd: tempDir,
			});
			await session.waitForIdle();

			const internals = session as unknown as {
				_maybeStageScratchHandoffCloseout(): Promise<boolean>;
				_scratchCloseout?: { displayPath: string; baselineText: string | undefined };
			};
			await expect(internals._maybeStageScratchHandoffCloseout()).resolves.toBe(true);
			const stagedPath = internals._scratchCloseout!.displayPath;

			// The path pin persists before the closeout turn runs.
			const pinEntry = session.sessionManager
				.getBranch()
				.find((e) => e.type === "custom" && (e as { customType?: string }).customType === "scratch-handoff-path");
			expect((pinEntry as { data?: { path?: string } } | undefined)?.data?.path).toBe(stagedPath);

			// Simulate the closeout turn writing the checkpoint.
			const fs = await import("node:fs");
			fs.mkdirSync(dirname(stagedPath), { recursive: true });
			fs.writeFileSync(stagedPath, "* TODO Work\n- Objective: Finish\n- Next action: Verify\n");

			const result = await session.compact();
			expect(result.summary).toContain("Scratch handoff");

			const entries = session.sessionManager.getBranch();
			const writeMarkers = entries.filter(
				(e) => e.type === "custom" && (e as { customType?: string }).customType === "scratch-handoff-write",
			);
			expect(writeMarkers.length).toBe(1);
			const readMarker = [...entries]
				.reverse()
				.find((e) => e.type === "custom_message" && e.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE);
			expect(readMarker).toBeDefined();
			const resumedText = JSON.stringify(session.agent.state.messages);
			expect(resumedText).toContain("- Next action: Verify");

			// While the staged turn is still queued, staging holds compaction.
			internals._scratchCloseout = undefined;
			await expect(internals._maybeStageScratchHandoffCloseout()).resolves.toBe(true);
			await session.waitForIdle();
			await expect(internals._maybeStageScratchHandoffCloseout()).resolves.toBe(false);

			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});
});
