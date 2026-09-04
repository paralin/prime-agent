import type { UserMessage } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { buildSessionHistorySnapshot, layoutHistoryText } from "../src/core/history-snapshot.js";
import type { SessionEntry, SessionEntryBase } from "../src/core/session-manager.js";

function userEntry(id: string, text: string): SessionEntry {
	const base: SessionEntryBase = { type: "test", id, parentId: null, timestamp: "2026-09-03T00:00:00.000Z" };
	const message: UserMessage = { role: "user", content: text, timestamp: 0 };
	return { ...base, type: "message", message };
}

describe("history snapshots", () => {
	it("renders deterministic bounded PNG pages with archive metrics", () => {
		const entries = [userEntry("one", "first fact"), userEntry("two", "second fact")];
		const first = buildSessionHistorySnapshot({ entries });
		const second = buildSessionHistorySnapshot({ entries });

		expect(first.text).toContain("first fact");
		expect(first.text).toContain("second fact");
		expect(first.messageCount).toBe(2);
		expect(first.truncated).toBe(false);
		expect(first.images).toEqual(second.images);
		expect(first.images).toHaveLength(1);
		expect(Buffer.from(first.images[0]?.data ?? "", "base64").subarray(0, 8)).toEqual(
			Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
		);
	});

	it("re-renders two generations from chronological source rather than prior images", () => {
		const first = buildSessionHistorySnapshot({ entries: [userEntry("one", "fact before first boundary")] });
		const second = buildSessionHistorySnapshot({
			entries: [userEntry("two", "fact after first boundary")],
			previous: first,
		});

		expect(second.text).toContain("fact before first boundary");
		expect(second.text).toContain("fact after first boundary");
		expect(second.messageCount).toBe(2);
		expect(second.images[0]?.data).not.toBe(first.images[0]?.data);
		expect(second.text).not.toContain("image/png");
	});

	it("preserves the newest chronology when the image budget is exhausted", () => {
		const history = buildSessionHistorySnapshot({
			entries: [userEntry("large", `${"old line\n".repeat(20_000)}NEWEST_FACT`)],
		});

		expect(history.truncated).toBe(true);
		expect(history.images).toHaveLength(8);
		expect(layoutHistoryText(history.text).pages.at(-1)).toContain("NEWEST_FACT");
	});
});
