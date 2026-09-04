import { describe, expect, it } from "vitest";
import {
	buildScratchHandoffContinuation,
	hasCommittedScratchHandoff,
	latestPersistedScratchHandoffPath,
	resolveScratchHandoffBoundary,
	resolveScratchHandoffPath,
	resolveScratchHandoffRoute,
	SCRATCH_HANDOFF_CONTINUE_INSTRUCTION,
	SCRATCH_HANDOFF_PATH_CUSTOM_TYPE,
	SCRATCH_HANDOFF_READ_CUSTOM_TYPE,
	scratchHandoffCompactionDetails,
} from "../src/core/compaction/scratch-handoff.js";
import { renderScratchHandoffCloseoutMessage } from "../src/core/prompts/scratch-handoff.js";
import type { SessionEntry } from "../src/core/session-manager.js";

let nextId = 1;
function entry(partial: Record<string, unknown> & { id?: string }): SessionEntry {
	return {
		id: partial.id ?? `e${nextId++}`,
		parentId: "root",
		timestamp: new Date().toISOString(),
		...partial,
	} as SessionEntry;
}

function pathPin(path: string): SessionEntry {
	return entry({ type: "custom", customType: SCRATCH_HANDOFF_PATH_CUSTOM_TYPE, data: { path } });
}

function legacyReadMarker(path: string): SessionEntry {
	return entry({
		type: "custom_message",
		customType: SCRATCH_HANDOFF_READ_CUSTOM_TYPE,
		content: "legacy resume",
		display: false,
		details: { path },
	});
}

function scratchCompaction(path: string): SessionEntry {
	return entry({
		type: "compaction",
		summary: "",
		firstKeptEntryId: "continuation",
		tokensBefore: 100,
		details: scratchHandoffCompactionDetails(path, {
			text: "history",
			images: [],
			messageCount: 2,
			truncated: false,
		}),
	});
}

describe("resolveScratchHandoffPath", () => {
	it("builds a dated per-session path and sanitizes ids", () => {
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

describe("persisted scratch identity", () => {
	it("prefers the newest committed compaction while accepting legacy read markers and path pins", () => {
		expect(latestPersistedScratchHandoffPath([legacyReadMarker("legacy.org")])).toBe("legacy.org");
		expect(latestPersistedScratchHandoffPath([pathPin("pinned.org")])).toBe("pinned.org");
		expect(
			latestPersistedScratchHandoffPath([
				legacyReadMarker("legacy.org"),
				pathPin("pinned.org"),
				scratchCompaction("current.org"),
			]),
		).toBe("current.org");
	});

	it("uses committed state, not a path pin, to choose the later closeout", () => {
		expect(hasCommittedScratchHandoff([pathPin("work.org")], "work.org")).toBe(false);
		expect(hasCommittedScratchHandoff([legacyReadMarker("work.org")], "work.org")).toBe(true);
		expect(hasCommittedScratchHandoff([scratchCompaction("work.org")], "work.org")).toBe(true);
	});
});

describe("scratch handoff messages", () => {
	it("discloses omitted image history without truncating the checkpoint", () => {
		const message = buildScratchHandoffContinuation({
			displayPath: "agent/work.org",
			scratchText: "* TODO Current task",
			history: { text: "full source", images: [], messageCount: 100, truncated: true },
		});
		expect(message.content).toEqual([
			{
				type: "text",
				text: expect.stringContaining("not the complete transcript"),
			},
		]);
		expect(JSON.stringify(message.content)).toContain("* TODO Current task");
	});
	it("uses the exact first and later closeout prompts", () => {
		expect(renderScratchHandoffCloseoutMessage("agent/x.org", true)).toBe(
			"Stop working for now; please create a .org file brain-dump of your ongoing work to agent/x.org, use org-todo structure including TODO subheadings, subheadings of subheadings, TODOs on nested subheadings, and so on. It should be detailed enough to hand off this work to a colleague.",
		);
		expect(renderScratchHandoffCloseoutMessage("agent/x.org", false)).toBe(
			"Stop working for now and make any final edits to agent/x.org such that you can hand it to a colleague to continue this work.",
		);
	});

	it("orders snapshot images before the complete labelled Org file and final instruction", () => {
		const continuation = buildScratchHandoffContinuation({
			displayPath: 'agent/a&"b.org',
			scratchText: "* TODO Parent\n** TODO Nested\nfull tail",
			history: {
				text: "history",
				images: [{ type: "image", mimeType: "image/png", data: "abc" }],
				messageCount: 1,
				truncated: false,
			},
			timestamp: 1,
		});

		expect(continuation.role).toBe("user");
		expect(continuation.content[0]).toMatchObject({ type: "image", data: "abc" });
		const text = continuation.content[1];
		expect(text).toMatchObject({ type: "text" });
		if (!text || typeof text === "string" || text.type !== "text") {
			throw new Error("Expected text after snapshot images");
		}
		expect(text.text).toContain('<scratch-handoff-file path="agent/a&amp;&quot;b.org">');
		expect(text.text).toContain("** TODO Nested\nfull tail");
		expect(text.text).toContain("Earlier conversation turns were compacted, not lost");
		expect(text.text).toContain("later user messages take precedence");
		expect(text.text).toContain("only within the active user-authorized task");
		expect(text.text.endsWith(SCRATCH_HANDOFF_CONTINUE_INSTRUCTION)).toBe(true);
	});
});

describe("scratch handoff routing", () => {
	const route = (
		strategy: "default" | "scratch-handoff" | "native-or-scratch",
		supportsNativeCompaction: boolean,
		supportsImages = true,
	) =>
		resolveScratchHandoffRoute({
			strategy,
			scratchEnabled: true,
			supportsNativeCompaction,
			supportsImages,
		});

	it.each([
		["scratch-handoff", true, "scratch"],
		["scratch-handoff", false, "scratch"],
		["native-or-scratch", true, "ordinary"],
		["native-or-scratch", false, "scratch"],
		["default", true, "ordinary"],
		["default", false, "ordinary"],
	] as const)("routes %s with native=%s to %s", (strategy, supportsNativeCompaction, expected) => {
		expect(route(strategy, supportsNativeCompaction).mode).toBe(expected);
	});

	it("falls back visibly for a text-only model", () => {
		const selected = route("scratch-handoff", false, false);
		expect(selected.mode).toBe("ordinary");
		expect(selected.warning).toContain("vision-capable model");
	});

	it("requires scratch handoff to be enabled", () => {
		expect(
			resolveScratchHandoffRoute({
				strategy: "scratch-handoff",
				scratchEnabled: false,
				supportsNativeCompaction: false,
				supportsImages: true,
			}).mode,
		).toBe("ordinary");
	});

	it.each([
		["manual", "scratch", true],
		["threshold", "scratch", true],
		["requested", "scratch", true],
		["overflow", "ordinary", false],
	] as const)("routes a %s boundary to %s with closeout=%s", (reason, mode, requiresCloseout) => {
		expect(
			resolveScratchHandoffBoundary({
				strategy: "scratch-handoff",
				scratchEnabled: true,
				supportsNativeCompaction: true,
				supportsImages: true,
				reason,
			}),
		).toMatchObject({ mode, requiresCloseout });
	});
});
