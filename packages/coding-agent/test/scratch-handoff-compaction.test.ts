import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fauxAssistantMessage, registerFauxProvider, type TextContent } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import {
	resolveScratchHandoffPath,
	SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE,
	SCRATCH_HANDOFF_CONTINUE_INSTRUCTION,
	type ScratchHandoffCompactionDetails,
} from "../src/core/compaction/scratch-handoff.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { createAgentSession } from "../src/core/sdk.js";
import { type CompactionEntry, SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { createTestExtensionsResult, createTestResourceLoader, createTestSessionManager } from "./utilities.js";

describe("scratch handoff compaction", () => {
	it("commits two ordered image-plus-Org continuations and reconstructs the latest after restart", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-handoff-"));
		let nativeCompactionCalls = 0;
		const faux = registerFauxProvider({
			compact: async () => {
				nativeCompactionCalls++;
				throw new Error("explicit scratch handoff must not call provider-native compaction");
			},
		});
		try {
			const rootDir = join(tempDir, "scratch");
			const dailyLogPath = join(tempDir, "daily.org");
			const seenCloseouts: string[] = [];
			let expectedPath = "";
			faux.setResponses([
				fauxAssistantMessage("fact before first boundary"),
				(context) => {
					seenCloseouts.push(JSON.stringify(context.messages.at(-1)));
					mkdirSync(dirname(expectedPath), { recursive: true });
					writeFileSync(
						expectedPath,
						"* TODO Ship scratch handoff\n** TODO Implement boundary\n*** TODO Record daily log\n** TODO Prove second boundary\n",
					);
					return fauxAssistantMessage("first checkpoint written");
				},
				() => {
					writeFileSync(dailyLogPath, "* Implemented boundary\nMoved implementation notes from scratch.\n");
					writeFileSync(
						expectedPath,
						`* TODO Ship scratch handoff\n** DONE Implement boundary\n*** DONE Record daily log\n[[file:${dailyLogPath}::*Implemented boundary][daily log]]\n** TODO Prove second boundary\n`,
					);
					return fauxAssistantMessage("fact after first boundary");
				},
				(context) => {
					seenCloseouts.push(JSON.stringify(context.messages.at(-1)));
					writeFileSync(
						expectedPath,
						`* DONE Ship scratch handoff\n** DONE Implement boundary\n*** DONE Record daily log\n[[file:${dailyLogPath}::*Implemented boundary][daily log]]\n** DONE Prove second boundary\n`,
					);
					return fauxAssistantMessage("second checkpoint written");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, native: true, strategy: "scratch-handoff" },
				scratchHandoff: { enabled: true, rootDir },
			});
			const sessionManager = createTestSessionManager(tempDir);
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager,
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

			expect(existsSync(expectedPath)).toBe(false);
			expect(session.agent.state.systemPrompt).not.toContain("Scratch continuity");

			await session.prompt("do the first piece of work");
			await session.waitForIdle();
			await session.compact();

			expect(seenCloseouts[0]).toContain(
				`Stop working for now; please create a .org file brain-dump of your ongoing work to ${expectedPath}`,
			);
			assertContinuation(session.agent.state.messages, expectedPath, "** TODO Prove second boundary");

			await session.prompt("do the second piece of work");
			await session.waitForIdle();
			expect(readFileSync(expectedPath, "utf8")).toContain("*** DONE Record daily log");
			expect(readFileSync(expectedPath, "utf8")).toContain(
				`[[file:${dailyLogPath}::*Implemented boundary][daily log]]`,
			);
			expect(readFileSync(dailyLogPath, "utf8")).toContain("Moved implementation notes from scratch.");
			await session.compact();

			expect(seenCloseouts[1]).toContain(
				`Stop working for now and make any final edits to ${expectedPath} such that you can hand it to a colleague to continue this work.`,
			);
			assertContinuation(session.agent.state.messages, expectedPath, "* DONE Ship scratch handoff");
			assertContinuation(session.agent.state.messages, expectedPath, "** DONE Prove second boundary");
			expect(nativeCompactionCalls).toBe(0);

			const compactions = session.sessionManager
				.getBranch()
				.filter((entry): entry is CompactionEntry<ScratchHandoffCompactionDetails> => entry.type === "compaction");
			expect(compactions).toHaveLength(2);
			expect(compactions.every((entry) => entry.summary === "")).toBe(true);
			const latestHistory = compactions.at(-1)?.details?.scratchHandoff.historyText ?? "";
			expect(latestHistory).toContain("fact before first boundary");
			expect(latestHistory).toContain("fact after first boundary");
			expect(latestHistory).not.toContain("Stop working for now");

			const sessionFile = session.sessionManager.getSessionFile();
			expect(sessionFile).toBeDefined();
			await session.disposeAsync();

			const reopenedManager = SessionManager.open(sessionFile!);
			const { session: reopened } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: reopenedManager,
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
				noTools: "all",
				cwd: tempDir,
			});
			assertContinuation(reopened.agent.state.messages, expectedPath, "* DONE Ship scratch handoff");
			assertContinuation(reopened.agent.state.messages, expectedPath, "** DONE Prove second boundary");
			await reopened.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("runs the scratch closeout before the /compact session command", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-command-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			let expectedPath = "";
			let closeoutCalls = 0;
			faux.setResponses([
				fauxAssistantMessage("work before the command boundary"),
				() => {
					closeoutCalls++;
					mkdirSync(dirname(expectedPath), { recursive: true });
					writeFileSync(expectedPath, "* TODO Continue after the command boundary\n");
					return fauxAssistantMessage("command checkpoint written");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff" },
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
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

			await session.prompt("do work before the command boundary");
			await session.prompt("/compact");
			await session.waitForIdle();

			expect(closeoutCalls).toBe(1);
			const branchJson = JSON.stringify(session.sessionManager.getBranch());
			expect(
				session.sessionManager
					.getBranch()
					.filter(
						(entry) =>
							entry.type === "custom_message" && entry.customType === SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE,
					),
			).toHaveLength(1);
			expect(branchJson).not.toContain('"role":"user","content":[{"type":"text","text":"Stop working for now');
			expect(session.sessionManager.getBranch().filter((entry) => entry.type === "compaction")).toHaveLength(1);
			assertContinuation(session.agent.state.messages, expectedPath, "* TODO Continue after the command boundary");
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("runs scratch closeout through before_agent_start extensions", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-extension-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			let expectedPath = "";
			const extensionPrompts: string[] = [];
			let closeoutSystemPrompt = "";
			let sawExtensionContext = false;
			const extensionsResult = await createTestExtensionsResult(
				[
					(pi) => {
						pi.on("before_agent_start", async (event) => {
							extensionPrompts.push(event.prompt);
							if (!event.prompt.startsWith("Stop working for now")) return;
							return {
								message: {
									customType: "scratch-closeout-extension",
									content: "extension closeout context",
									display: true,
									details: { source: "test" },
								},
								systemPrompt: `${event.systemPrompt}\n\nextension closeout instructions`,
							};
						});
					},
				],
				tempDir,
			);
			faux.setResponses([
				fauxAssistantMessage("work before the extension boundary"),
				(context) => {
					closeoutSystemPrompt = context.systemPrompt ?? "";
					sawExtensionContext = context.messages.some(
						(message) =>
							message.role === "user" &&
							typeof message.content !== "string" &&
							message.content.some((part) => part.type === "text" && part.text === "extension closeout context"),
					);
					mkdirSync(dirname(expectedPath), { recursive: true });
					writeFileSync(expectedPath, "* TODO Continue after extension closeout\n");
					return fauxAssistantMessage("extension checkpoint written");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff" },
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader({ extensionsResult }),
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

			await session.prompt("do work before the extension boundary");
			await session.compact();

			expect(extensionPrompts).toContain(
				`Stop working for now; please create a .org file brain-dump of your ongoing work to ${expectedPath}, use org-todo structure including TODO subheadings, subheadings of subheadings, TODOs on nested subheadings, and so on. It should be detailed enough to hand off this work to a colleague.`,
			);
			expect(closeoutSystemPrompt).toContain("extension closeout instructions");
			expect(sawExtensionContext).toBe(true);
			expect(
				session.sessionManager
					.getBranch()
					.some((entry) => entry.type === "custom_message" && entry.customType === "scratch-closeout-extension"),
			).toBe(true);
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it.each([
		["cancelled", { stopReason: "aborted" as const }],
		["failed", { stopReason: "error" as const, errorMessage: "closeout provider failed" }],
		["exhausted", { stopReason: "length" as const }],
	])("does not retry or compact after a %s /compact closeout", async (_label, closeoutResult) => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-command-failure-"));
		const faux = registerFauxProvider();
		try {
			let closeoutCalls = 0;
			faux.setResponses([
				fauxAssistantMessage("work remains authoritative"),
				() => {
					closeoutCalls++;
					return fauxAssistantMessage("", closeoutResult);
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff" },
				retry: { enabled: false },
				scratchHandoff: { enabled: true, rootDir: join(tempDir, "scratch") },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
				noTools: "all",
				cwd: tempDir,
			});

			const checkpoint = resolveScratchHandoffPath({
				cwd: tempDir,
				rootDir: join(tempDir, "scratch"),
				sessionId: session.sessionId,
			}).absolutePath;
			mkdirSync(dirname(checkpoint), { recursive: true });
			writeFileSync(checkpoint, "* TODO Old checkpoint must not authorize a failed closeout\n");
			await session.prompt("do work before the failed command boundary");
			await session.prompt("/compact").catch(() => undefined);
			await session.waitForIdle();

			expect(closeoutCalls).toBe(1);
			expect(session.unfinishedActionCount).toBe(0);
			expect(session.sessionManager.getBranch().some((entry) => entry.type === "compaction")).toBe(false);
			expect(JSON.stringify(session.agent.state.messages)).toContain("work remains authoritative");
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it.each([
		["successful", { stopReason: "stop" as const }, true],
		["cancelled", { stopReason: "aborted" as const }, false],
		["failed", { stopReason: "error" as const, errorMessage: "closeout provider failed" }, false],
	])("binds /compact to an already-pending %s threshold closeout", async (_label, closeoutResult, compacted) => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-command-threshold-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			let releaseWork = () => {};
			const workGate = new Promise<void>((resolve) => {
				releaseWork = resolve;
			});
			let expectedPath = "";
			let closeoutCalls = 0;
			faux.setResponses([
				async () => {
					await workGate;
					return fauxAssistantMessage("threshold work remains authoritative");
				},
				() => {
					closeoutCalls++;
					if (closeoutResult.stopReason === "stop") {
						mkdirSync(dirname(expectedPath), { recursive: true });
						writeFileSync(expectedPath, "* TODO Continue after the shared closeout\n");
					}
					return fauxAssistantMessage(
						closeoutResult.stopReason === "stop" ? "checkpoint written" : "",
						closeoutResult,
					);
				},
				() => {
					closeoutCalls++;
					mkdirSync(dirname(expectedPath), { recursive: true });
					writeFileSync(expectedPath, "* TODO Unexpected second closeout\n");
					return fauxAssistantMessage("unexpected second closeout succeeded");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff", triggerContextTokens: 1 },
				retry: { enabled: false },
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
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

			const work = session.prompt("do work before the threshold boundary");
			await vi.waitFor(() => expect(session.isStreaming).toBe(true));
			const compact = session.prompt("/compact", { streamingBehavior: "steer" });
			releaseWork();
			await Promise.allSettled([work, compact]);
			await session.waitForIdle();

			expect(closeoutCalls).toBe(1);
			expect(session.unfinishedActionCount).toBe(0);
			expect(session.sessionManager.getBranch().some((entry) => entry.type === "compaction")).toBe(compacted);
			if (compacted) {
				assertContinuation(session.agent.state.messages, expectedPath, "* TODO Continue after the shared closeout");
			} else {
				expect(JSON.stringify(session.agent.state.messages)).toContain("threshold work remains authoritative");
			}
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("waits for each automatic threshold closeout before compacting", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-threshold-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			let expectedPath = "";
			let closeoutCalls = 0;
			faux.setResponses([
				fauxAssistantMessage("first generation fact"),
				() => {
					closeoutCalls++;
					mkdirSync(dirname(expectedPath), { recursive: true });
					writeFileSync(expectedPath, "* TODO Cross two thresholds\n** TODO Finish peer\n");
					return fauxAssistantMessage("first threshold checkpoint written");
				},
				() => fauxAssistantMessage("second generation fact"),
				() => {
					closeoutCalls++;
					writeFileSync(expectedPath, "* DONE Cross two thresholds\n** DONE Finish peer\n");
					return fauxAssistantMessage("second threshold checkpoint written");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: {
					enabled: true,
					native: false,
					strategy: "scratch-handoff",
					triggerContextTokens: 1,
				},
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
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

			await session.prompt("first threshold task");
			await session.waitForIdle();
			expect(closeoutCalls).toBe(1);
			expect(session.sessionManager.getBranch().filter((entry) => entry.type === "compaction")).toHaveLength(1);

			await session.prompt("second threshold task");
			await session.waitForIdle();
			expect(closeoutCalls).toBe(2);
			const compactions = session.sessionManager
				.getBranch()
				.filter((entry): entry is CompactionEntry<ScratchHandoffCompactionDetails> => entry.type === "compaction");
			expect(compactions).toHaveLength(2);
			expect(compactions.every((entry) => entry.summary === "")).toBe(true);
			expect(compactions.at(-1)?.details?.scratchHandoff.historyText).toContain("second generation fact");
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("leaves the old context authoritative when closeout produces no checkpoint", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-missing-"));
		const faux = registerFauxProvider();
		try {
			faux.setResponses([
				fauxAssistantMessage("work remains visible"),
				fauxAssistantMessage("did not write the checkpoint"),
				fauxAssistantMessage("still did not write the checkpoint"),
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff" },
				scratchHandoff: { enabled: true, rootDir: join(tempDir, "scratch") },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
				noTools: "all",
				cwd: tempDir,
			});

			await session.prompt("first piece of work");
			await session.waitForIdle();
			await expect(session.compact()).rejects.toThrow("did not produce a non-empty Org checkpoint");

			expect(session.sessionManager.getBranch().some((entry) => entry.type === "compaction")).toBe(false);
			expect(JSON.stringify(session.agent.state.messages)).toContain("work remains visible");
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("gives a missing checkpoint one repair turn inside the same compaction episode", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-repair-"));
		const faux = registerFauxProvider();
		try {
			const rootDir = join(tempDir, "scratch");
			let expectedPath = "";
			const seenLastMessages: string[] = [];
			faux.setResponses([
				fauxAssistantMessage("work remains visible"),
				(context) => {
					seenLastMessages.push(JSON.stringify(context.messages.at(-1)));
					return fauxAssistantMessage("I stopped before writing it");
				},
				(context) => {
					seenLastMessages.push(JSON.stringify(context.messages.at(-1)));
					mkdirSync(dirname(expectedPath), { recursive: true });
					writeFileSync(expectedPath, "* TODO Continue after repaired handoff\n");
					return fauxAssistantMessage("checkpoint repaired");
				},
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff" },
				scratchHandoff: { enabled: true, rootDir },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
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

			await session.prompt("first piece of work");
			await session.compact();

			expect(seenLastMessages).toHaveLength(2);
			expect(seenLastMessages[1]).toContain("still missing or empty");
			expect(
				session.sessionManager
					.getBranch()
					.filter(
						(entry) =>
							entry.type === "custom_message" && entry.customType === SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE,
					),
			).toHaveLength(2);
			assertContinuation(session.agent.state.messages, expectedPath, "* TODO Continue after repaired handoff");
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("does not commit a boundary after the closeout turn is cancelled", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-cancelled-"));
		const faux = registerFauxProvider();
		try {
			faux.setResponses([
				fauxAssistantMessage("work remains visible"),
				fauxAssistantMessage("", { stopReason: "aborted" }),
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff" },
				scratchHandoff: { enabled: true, rootDir: join(tempDir, "scratch") },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
				noTools: "all",
				cwd: tempDir,
			});

			await session.prompt("first piece of work");
			await session.waitForIdle();
			await expect(session.compact()).rejects.toThrow("closeout did not run");
			expect(session.sessionManager.getBranch().some((entry) => entry.type === "compaction")).toBe(false);
			expect(JSON.stringify(session.agent.state.messages)).toContain("work remains visible");
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("abortCompaction cancels the active closeout episode", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-scratch-abort-closeout-"));
		const faux = registerFauxProvider();
		try {
			let closeoutStarted = () => {};
			const started = new Promise<void>((resolve) => {
				closeoutStarted = resolve;
			});
			faux.setResponses([
				fauxAssistantMessage("work remains visible"),
				(_context, options) =>
					new Promise((resolve) => {
						closeoutStarted();
						options?.signal?.addEventListener(
							"abort",
							() => resolve(fauxAssistantMessage("", { stopReason: "aborted" })),
							{ once: true },
						);
					}),
			]);
			const authStorage = AuthStorage.inMemory();
			authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
			const settingsManager = SettingsManager.inMemory();
			settingsManager.applyOverrides({
				compaction: { enabled: true, strategy: "scratch-handoff" },
				scratchHandoff: { enabled: true, rootDir: join(tempDir, "scratch") },
			});
			const { session } = await createAgentSession({
				model: faux.getModel(),
				sessionManager: createTestSessionManager(tempDir),
				settingsManager,
				resourceLoader: createTestResourceLoader(),
				authStorage,
				modelRegistry: ModelRegistry.inMemory(authStorage),
				noTools: "all",
				cwd: tempDir,
			});

			await session.prompt("first piece of work");
			const compact = session.compact();
			await started;
			session.abortCompaction();

			await expect(compact).rejects.toThrow("Compaction cancelled");
			expect(session.sessionManager.getBranch().some((entry) => entry.type === "compaction")).toBe(false);
			expect(JSON.stringify(session.agent.state.messages)).toContain("work remains visible");
			await session.disposeAsync();
		} finally {
			faux.unregister();
			rmSync(tempDir, { recursive: true, force: true });
		}
	});
});

function assertContinuation(
	messages: readonly { role: string; content?: unknown }[],
	path: string,
	orgNeedle: string,
): void {
	expect(messages).toHaveLength(1);
	const message = messages[0];
	expect(message?.role).toBe("user");
	const content = message?.content;
	expect(Array.isArray(content)).toBe(true);
	if (!Array.isArray(content)) throw new Error("Expected image-and-text continuation content");
	expect(content.length).toBeGreaterThan(1);
	expect(content.slice(0, -1).every((part) => part?.type === "image")).toBe(true);
	const last = content.at(-1) as TextContent | undefined;
	expect(last?.type).toBe("text");
	expect(last?.text).toContain(`<scratch-handoff-file path="${path}">`);
	expect(last?.text).toContain(orgNeedle);
	expect(last?.text.endsWith(SCRATCH_HANDOFF_CONTINUE_INSTRUCTION)).toBe(true);
	expect(last?.text).not.toContain(["recent", "session", "context"].join("-"));
}
