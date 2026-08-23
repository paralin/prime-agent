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
	it("/compact rebuilds around the existing checkpoint without another model turn", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-handoff-"));
		let nativeCompactionCalls = 0;
		const faux = registerFauxProvider({
			compact: async () => {
				nativeCompactionCalls++;
				throw new Error("manual compaction must use the scratch handoff");
			},
		});
		try {
			const rootDir = join(tempDir, "scratch");
			let providerCalls = 0;
			faux.setResponses([
				() => {
					providerCalls++;
					return fauxAssistantMessage("work done");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, native: true, strategy: "native-or-scratch" },
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
			const expectedPath = resolveScratchHandoffPath({
				cwd: tempDir,
				rootDir,
				sessionId: session.sessionId,
			}).absolutePath;
			mkdirSync(join(expectedPath, ".."), { recursive: true });
			writeFileSync(expectedPath, "* TODO Work\n- Objective: Finish\n- Next action: Verify\n");

			await session.prompt("do some work");
			await session.waitForIdle();

			const result = await session.compact();
			expect(result.summary).toContain("Scratch handoff");
			expect(providerCalls).toBe(1);
			expect(nativeCompactionCalls).toBe(0);

			const entries = session.sessionManager.getBranch();
			const readMarker = [...entries]
				.reverse()
				.find((entry) => entry.type === "custom_message" && entry.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE);
			expect(readMarker).toBeDefined();
			const details = (readMarker as { details?: { path?: string } }).details;
			expect(details?.path).toBe(expectedPath);
			expect(
				entries.find((entry) => entry.type === "compaction" && entry.firstKeptEntryId === readMarker!.id),
			).toBeDefined();

			const resumedText = JSON.stringify(session.agent.state.messages);
			expect(resumedText).toContain("- Next action: Verify");
			expect(resumedText).not.toContain("PENCILS DOWN");

			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("/compact uses recent context when no checkpoint exists instead of standard summarization", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-missing-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			let providerCalls = 0;
			faux.setResponses([
				() => {
					providerCalls++;
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

			await session.prompt("first piece of work");
			await session.waitForIdle();
			const result = await session.compact();

			expect(providerCalls).toBe(1);
			expect(result.summary).toContain("Scratch handoff");
			const readMarker = session.sessionManager
				.getBranch()
				.find((entry) => entry.type === "custom_message" && entry.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE);
			expect(readMarker).toBeDefined();
			const resumedText = JSON.stringify(session.agent.state.messages);
			expect(resumedText).toContain("first piece of work");
			expect(resumedText).not.toContain("PENCILS DOWN");

			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});
});
