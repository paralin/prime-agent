import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
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
});
