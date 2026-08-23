import { appendFileSync, mkdtempSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { readSessionInfo } from "../src/core/session-manager.js";

const tempDirs: string[] = [];

afterEach(() => {
	for (const directory of tempDirs.splice(0)) rmSync(directory, { recursive: true, force: true });
});

describe("readSessionInfo", () => {
	it("shares an in-flight scan for concurrent readers of the same transcript", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-session-info-single-flight-"));
		tempDirs.push(directory);
		const path = join(directory, "session.jsonl");
		writeFileSync(
			path,
			`${JSON.stringify({
				type: "session",
				version: 3,
				id: "session",
				timestamp: "2026-01-01T00:00:00.000Z",
				cwd: directory,
			})}\n`,
		);

		const first = readSessionInfo(path);
		const second = readSessionInfo(path);

		expect(second).toBe(first);
		await expect(first).resolves.toMatchObject({ id: "session", path });
	});

	it("updates cached metadata from appended entries", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-session-info-append-"));
		tempDirs.push(directory);
		const path = join(directory, "session.jsonl");
		writeFileSync(
			path,
			`${JSON.stringify({
				type: "session",
				version: 3,
				id: "session",
				timestamp: "2026-01-01T00:00:00.000Z",
				cwd: directory,
			})}\n`,
		);

		await expect(readSessionInfo(path)).resolves.toMatchObject({ messageCount: 0, name: undefined });
		appendFileSync(
			path,
			`${JSON.stringify({
				type: "message",
				id: "message",
				parentId: null,
				timestamp: "2026-01-02T00:00:00.000Z",
				message: { role: "user", content: "hello", timestamp: Date.parse("2026-01-02T00:00:00.000Z") },
			})}\n${JSON.stringify({
				type: "session_info",
				id: "info",
				parentId: "message",
				timestamp: "2026-01-02T00:00:01.000Z",
				name: "renamed",
			})}\n`,
		);

		await expect(readSessionInfo(path)).resolves.toMatchObject({
			messageCount: 1,
			conversationMessageCount: 1,
			firstMessage: "hello",
			name: "renamed",
		});
	});

	it("falls back to a full scan after atomic replacement", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-session-info-replace-"));
		tempDirs.push(directory);
		const path = join(directory, "session.jsonl");
		const replacement = join(directory, "replacement.jsonl");
		const header = {
			type: "session",
			version: 3,
			id: "session",
			timestamp: "2026-01-01T00:00:00.000Z",
			cwd: directory,
		};
		writeFileSync(path, `${JSON.stringify(header)}\n`);
		await expect(readSessionInfo(path)).resolves.toMatchObject({ messageCount: 0 });

		writeFileSync(
			replacement,
			`${JSON.stringify(header)}\n${JSON.stringify({
				type: "message",
				id: "replacement-message",
				parentId: null,
				timestamp: "2026-01-03T00:00:00.000Z",
				message: { role: "user", content: "replacement", timestamp: Date.parse("2026-01-03T00:00:00.000Z") },
			})}\n`,
		);
		renameSync(replacement, path);

		await expect(readSessionInfo(path)).resolves.toMatchObject({ messageCount: 1, firstMessage: "replacement" });
	});
});
