import type { AgentMessage } from "@earendil-works/pi-agent-core";
import { describe, expect, it } from "vitest";
import {
	buildScratchHandoffRecentContext,
	buildScratchHandoffResumeMessage,
	latestPersistedScratchHandoffPath,
	resolveScratchContinuityState,
	resolveScratchHandoffPath,
	SCRATCH_HANDOFF_READ_CUSTOM_TYPE,
	SCRATCH_HANDOFF_WRITE_CUSTOM_TYPE,
	type ScratchHandoffDelta,
	scratchHandoffBodyPreview,
	scratchHandoffIsComplete,
	scratchHandoffRecentContextBudget,
	shouldUseScratchHandoffFallback,
} from "../src/core/compaction/scratch-handoff.js";
import {
	renderScratchHandoffCloseoutMessage,
	renderScratchHandoffInstructions,
} from "../src/core/prompts/scratch-handoff.js";
import type { SessionEntry } from "../src/core/session-manager.js";

let nextId = 1;
function entry(partial: Record<string, unknown> & { id?: string }): SessionEntry {
	return {
		id: partial.id ?? `e${nextId++}`,
		parentId: "root",
		timestamp: new Date().toISOString(),
		...partial,
	} as unknown as SessionEntry;
}

function messageEntry(role: AgentMessage["role"], text: string): SessionEntry {
	const content = role === "assistant" ? [{ type: "text", text } as const] : text;
	return entry({
		type: "message",
		message: { role, content, timestamp: Date.now() } as unknown as AgentMessage,
	});
}

describe("scratchHandoffIsComplete", () => {
	it("requires one root TODO with its own objective and next action", () => {
		expect(
			scratchHandoffIsComplete(
				"* TODO Resume implementation\n- Objective: Finish compaction\n- Next action:\n  1. Run focused tests\n",
			),
		).toBe(true);
		expect(scratchHandoffIsComplete("- Objective: Missing TODO\n- Next action: Continue\n")).toBe(false);
		expect(scratchHandoffIsComplete("* TODO Missing next action\n- Objective: Continue\n")).toBe(false);
		expect(
			scratchHandoffIsComplete(
				"* TODO Empty current task\n- Objective: \n- Next action: \n* DONE Historical task\n- Objective: Old objective\n- Next action: Old action\n",
			),
		).toBe(false);
		expect(
			scratchHandoffIsComplete(
				"* TODO First task\n- Objective: First\n- Next action: Continue\n* TODO Ambiguous task\n- Objective: Second\n- Next action: Continue\n",
			),
		).toBe(false);
	});
});

describe("resolveScratchContinuityState", () => {
	const complete = "* TODO Work\n- Objective: Finish it\n- Next action: Run the tests\n";
	it("verified when the closeout wrote or the document covers the branch", () => {
		expect(
			resolveScratchContinuityState({
				scratchText: complete,
				closeoutWriteCompleted: true,
				hasRecordedWrite: false,
				hasDelta: true,
			}),
		).toBe("verified");
		expect(
			resolveScratchContinuityState({
				scratchText: complete,
				closeoutWriteCompleted: false,
				hasRecordedWrite: true,
				hasDelta: false,
			}),
		).toBe("verified");
	});
	it("stale when work landed after the last write", () => {
		expect(
			resolveScratchContinuityState({
				scratchText: complete,
				closeoutWriteCompleted: false,
				hasRecordedWrite: true,
				hasDelta: true,
			}),
		).toBe("stale");
	});
	it("unusable without a complete active TODO", () => {
		expect(
			resolveScratchContinuityState({
				scratchText: "#+TITLE: empty\n",
				closeoutWriteCompleted: true,
				hasRecordedWrite: true,
				hasDelta: false,
			}),
		).toBe("unusable");
	});
});

describe("resolveScratchHandoffPath", () => {
	it("builds a dated per-session path under the root dir and sanitizes ids", () => {
		const { displayPath } = resolveScratchHandoffPath({
			cwd: "/tmp/proj",
			rootDir: "agent",
			sessionId: "abc/def 123",
			date: new Date(2026, 1, 3),
		});
		expect(displayPath).toBe("agent/20260203/abc-def-123.org");
	});
	it("honors an explicit scratch file", () => {
		const { displayPath, absolutePath } = resolveScratchHandoffPath({
			cwd: "/tmp/proj",
			rootDir: "agent",
			sessionId: "s",
			scratchFile: "notes/state.org",
		});
		expect(displayPath).toBe("notes/state.org");
		expect(absolutePath.startsWith("/tmp/proj")).toBe(true);
	});
});

describe("scratchHandoffBodyPreview", () => {
	it("returns the full body inside the token budget", () => {
		const preview = scratchHandoffBodyPreview("* TODO small\n- Objective: x\n- Next action: y\n");
		expect(preview.truncated).toBe(false);
		expect(preview.text.endsWith("y\n") || preview.text.endsWith("y")).toBe(true);
	});
	it("keeps whole lines under a tiny budget and reports truncation", () => {
		const long = Array.from(
			{ length: 50 },
			(_, i) => `- Item line ${i} with some padding text to add length ${i}`,
		).join("\n");
		const preview = scratchHandoffBodyPreview(long, 20);
		expect(preview.truncated).toBe(true);
		expect(preview.text.length).toBeLessThan(long.length);
	});
});

describe("scratchHandoffRecentContextBudget", () => {
	it("floors at the minimum and scales with the window", () => {
		expect(scratchHandoffRecentContextBudget(0)).toBe(2048);
		expect(scratchHandoffRecentContextBudget(Number.NaN)).toBe(2048);
		expect(scratchHandoffRecentContextBudget(100_000)).toBe(10_000);
	});
});

function readMarker(path: string): SessionEntry {
	return entry({
		type: "custom_message",
		customType: SCRATCH_HANDOFF_READ_CUSTOM_TYPE,
		content: "resume",
		display: false,
		details: { path },
	});
}
function writeMarker(path: string): SessionEntry {
	return entry({ type: "custom", customType: SCRATCH_HANDOFF_WRITE_CUSTOM_TYPE, data: { path } });
}

describe("latestPersistedScratchHandoffPath", () => {
	it("returns the newest persisted read marker path", () => {
		const entries = [readMarker("a.org"), messageEntry("user", "hi"), readMarker("b.org")];
		expect(latestPersistedScratchHandoffPath(entries)).toBe("b.org");
	});
	it("is undefined without markers", () => {
		expect(latestPersistedScratchHandoffPath([messageEntry("user", "hi")])).toBeUndefined();
	});
});

describe("buildScratchHandoffRecentContext", () => {
	it("collects only work after the last write marker for that path", () => {
		const entries = [
			messageEntry("user", "old work"),
			writeMarker("s.org"),
			messageEntry("user", "new work"),
			writeMarker("other.org"),
			messageEntry("assistant", "newest answer"),
		];
		const delta = buildScratchHandoffRecentContext({ entries, scratchPath: "s.org" });
		expect(delta).toBeDefined();
		expect(delta!.text).toContain("new work");
		expect(delta!.text).toContain("[Assistant]: newest answer");
		expect(delta!.text).not.toContain("old work");
	});

	it("never reaches back across the latest compaction boundary", () => {
		const kept = messageEntry("user", "kept tail");
		const entries = [
			messageEntry("user", "pre-compaction work"),
			entry({ type: "compaction", summary: "old summary", firstKeptEntryId: kept.id, tokensBefore: 100 }),
			kept,
		];
		const delta = buildScratchHandoffRecentContext({ entries });
		expect(delta).toBeDefined();
		expect(delta!.text).toContain("kept tail");
		expect(delta!.text).not.toContain("pre-compaction work");
	});

	it("trims to the budget from the oldest side and discloses the drop", () => {
		const entries = [
			messageEntry("user", "first message with plenty of words to take space ".repeat(3)),
			messageEntry("user", "second message also fairly long with several words here too"),
			messageEntry("assistant", "final short answer"),
		];
		const delta: ScratchHandoffDelta | undefined = buildScratchHandoffRecentContext({ entries, maxTokens: 55 });
		expect(delta).toBeDefined();
		expect(delta!.bounded.length).toBeLessThan(delta!.text.length);
		expect(delta!.bounded).toContain("[Older session context dropped:");
		expect(delta!.bounded).toContain("final short answer");
	});

	it("is undefined when nothing is pending", () => {
		expect(buildScratchHandoffRecentContext({ entries: [] })).toBeUndefined();
	});
});

describe("resume and closeout prompts", () => {
	it("closeout names the exact path and the create/update mode", () => {
		expect(renderScratchHandoffCloseoutMessage("agent/x.org", true)).toContain('scratch = Path("agent/x.org")');
		expect(renderScratchHandoffCloseoutMessage("agent/x.org", false)).toContain("update that exact path");
	});

	it("missing checkpoint defers to live context and carries the delta", () => {
		const message = buildScratchHandoffResumeMessage({
			displayPath: "agent/x.org",
			scratchText: undefined,
			recentContext: { text: "full", bounded: "bounded delta" },
		});
		expect(message).toContain("No scratch checkpoint exists yet");
		expect(message).toContain("bounded delta");
	});

	it("present checkpoint embeds the body and instructs against rereading", () => {
		const message = buildScratchHandoffResumeMessage({
			displayPath: "agent/x.org",
			scratchText: "* TODO Work\n- Objective: Finish\n- Next action: Test\n",
		});
		expect(message).toContain("<scratch-handoff-context>");
		expect(message).toContain("- Next action: Test");
		expect(message).toContain("Do not reread");
	});

	it("system instructions describe the checkpoint contract", () => {
		const block = renderScratchHandoffInstructions({ displayPath: "agent/x.org", sessionId: "s1", exists: false });
		expect(block).toContain("`agent/x.org`");
		expect(block).toContain("File not created yet");
	});
});

describe("shouldUseScratchHandoffFallback", () => {
	it("prefers provider-native compaction (openai-codex) over scratch", () => {
		expect(
			shouldUseScratchHandoffFallback({
				strategy: "native-or-scratch",
				scratchEnabled: true,
				supportsNativeCompaction: true,
			}),
		).toBe(false);
	});
	it("falls back to scratch for models without native compaction", () => {
		expect(
			shouldUseScratchHandoffFallback({
				strategy: "native-or-scratch",
				scratchEnabled: true,
				supportsNativeCompaction: false,
			}),
		).toBe(true);
	});
	it("requires the strategy and opt-in settings", () => {
		expect(
			shouldUseScratchHandoffFallback({
				strategy: "default",
				scratchEnabled: true,
				supportsNativeCompaction: false,
			}),
		).toBe(false);
		expect(
			shouldUseScratchHandoffFallback({
				strategy: "native-or-scratch",
				scratchEnabled: false,
				supportsNativeCompaction: false,
			}),
		).toBe(false);
		expect(
			shouldUseScratchHandoffFallback({
				strategy: undefined,
				scratchEnabled: true,
				supportsNativeCompaction: false,
			}),
		).toBe(false);
	});
});
