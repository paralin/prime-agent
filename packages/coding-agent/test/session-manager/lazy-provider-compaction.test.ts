import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ProviderNativeCompactionResult } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { SessionManager } from "../../src/core/session-manager.js";
import { assistantMsg, userMsg } from "../utilities.js";

function nativeCompaction(label: string): ProviderNativeCompactionResult {
	return {
		provider: "openai",
		replacementHistory: [{ type: "message", label }],
		compactionItem: { type: "compaction", label },
	};
}

describe("SessionManager lazy provider compactions", () => {
	it("defers superseded native history and restores it when branching", () => {
		const tempDir = mkdtempSync(join(tmpdir(), "lazy-provider-compaction-"));
		try {
			const session = SessionManager.create(join(tempDir, "project"), join(tempDir, "sessions"));
			const firstMessage = session.appendMessage(userMsg("first"));
			session.appendMessage(assistantMsg("first response"));
			const firstCompaction = session.appendCompaction(
				"first summary",
				firstMessage,
				100,
				undefined,
				undefined,
				undefined,
				undefined,
				nativeCompaction("first"),
			);
			const secondMessage = session.appendMessage(userMsg("second"));
			session.appendMessage(assistantMsg("second response"));
			session.appendCompaction(
				"second summary",
				secondMessage,
				200,
				undefined,
				undefined,
				undefined,
				undefined,
				nativeCompaction("second"),
			);

			const reopened = SessionManager.open(session.getSessionFile()!);
			const oldEntry = reopened.getEntry(firstCompaction);
			expect(oldEntry?.type).toBe("compaction");
			if (oldEntry?.type !== "compaction") return;
			expect(Object.getOwnPropertyDescriptor(oldEntry, "providerNativeCompaction")?.get).toBeTypeOf("function");

			reopened.branch(firstCompaction);
			const context = reopened.buildSessionContext("openai");
			expect(context.messages[0]).toMatchObject({
				role: "compactionSummary",
				providerPayload: {
					type: "openaiResponsesHistory",
					items: [{ type: "message", label: "first" }],
				},
			});
			expect(Object.getOwnPropertyDescriptor(oldEntry, "providerNativeCompaction")?.get).toBeUndefined();
		} finally {
			rmSync(tempDir, { recursive: true, force: true });
		}
	});
});
