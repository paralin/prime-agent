import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fauxAssistantMessage, registerFauxProvider } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { resolveScratchHandoffPath, SCRATCH_HANDOFF_READ_CUSTOM_TYPE } from "../src/core/compaction/scratch-handoff.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { createAgentSession } from "../src/core/sdk.js";
import { SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { createTestResourceLoader } from "./utilities.js";

describe("scratch handoff compaction", () => {
	it("/compact runs the closeout turn and rebuilds around the updated checkpoint", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-handoff-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			const providerCalls: string[] = [];
			let expectedPath: string | undefined;
			faux.setResponses([
				(_context) => {
					providerCalls.push("work");
					return fauxAssistantMessage("work done");
				},
				(context) => {
					const promptText = JSON.stringify(context.messages);
					if (!promptText.includes("PENCILS DOWN")) {
						providerCalls.push("unexpected");
						return fauxAssistantMessage("not a closeout");
					}
					providerCalls.push("closeout");
					if (expectedPath) {
						mkdirSync(join(expectedPath, ".."), { recursive: true });
						writeFileSync(expectedPath, "* TODO Work\n- Objective: Finish\n- Next action: Verify\n");
					}
					return fauxAssistantMessage("checkpoint written");
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
			expectedPath = resolveScratchHandoffPath({
				cwd: tempDir,
				rootDir,
				sessionId: session.sessionId,
			}).absolutePath;

			await session.prompt("do some work");
			await session.waitForIdle();

			const result = await session.compact();
			expect(result.summary).toContain("Scratch handoff");
			expect(providerCalls).toEqual(["work", "closeout"]);

			const entries = session.sessionManager.getBranch();
			const readMarker = [...entries]
				.reverse()
				.find((e) => e.type === "custom_message" && e.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE);
			expect(readMarker).toBeDefined();
			const details = (readMarker as { details?: { path?: string } }).details;
			expect(details?.path).toBe(expectedPath);
			const compaction = entries.find((e) => e.type === "compaction" && e.firstKeptEntryId === readMarker!.id);
			expect(compaction).toBeDefined();

			const resumedText = JSON.stringify(session.agent.state.messages);
			expect(resumedText).toContain("- Next action: Verify");
			expect(resumedText).not.toContain('"role":"user"');

			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("/compact falls back to standard summarization when the closeout updates nothing", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-fallback-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			const providerCalls: string[] = [];
			faux.setResponses([
				(_context) => {
					providerCalls.push("work");
					return fauxAssistantMessage("work done");
				},
				(_context) => {
					providerCalls.push("work");
					return fauxAssistantMessage("more work done");
				},
				(context) => {
					providerCalls.push(
						JSON.stringify(context.messages).includes("PENCILS DOWN") ? "closeout" : "unexpected",
					);
					return fauxAssistantMessage("I did not write anything");
				},
				(_context) => {
					providerCalls.push("summarize");
					return fauxAssistantMessage("Concise summary of the session work.");
				},
				// Turn-prefix and branch summarization make further side requests.
				fauxAssistantMessage("Older turns summarized."),
				fauxAssistantMessage("Remaining turns summarized."),
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				// A tiny keep-recent window leaves older turns to summarize on the
				// fallback path; otherwise the short fixture session has nothing to compact.
				compaction: { enabled: true, strategy: "native-or-scratch", keepRecentTokens: 10 },
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

			await session.prompt("first piece of work");
			await session.waitForIdle();
			await session.prompt("second piece of work");
			await session.waitForIdle();

			const result = await session.compact();

			// Closeout turn ran, produced no document, and the runtime fell back
			// to the ordinary summarization compaction instead of rebuilding
			// around an empty checkpoint.
			expect(providerCalls).toEqual(["work", "work", "closeout", "summarize"]);
			const entries = session.sessionManager.getBranch();
			const readMarker = [...entries]
				.reverse()
				.find((e) => e.type === "custom_message" && e.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE);
			expect(readMarker).toBeUndefined();
			expect(result.summary).toContain("Concise summary of the session work.");

			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});
});
