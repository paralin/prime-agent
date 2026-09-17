import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.js";
import { canonicalSessionPath } from "../src/core/session-lease.js";
import { SessionManager } from "../src/core/session-manager.js";
import { DaemonCatalogClient } from "../src/modes/daemon/daemon-catalog-process.js";
import { RlmSpawnLedger } from "../src/modes/daemon/rlm-ledger.js";

describe("daemon catalog entrypoint", () => {
	it("starts the dedicated catalog process over IPC", async () => {
		const agentDir = mkdtempSync(join(tmpdir(), "pa-catalog-entry-"));
		const previousAgentDir = process.env[ENV_AGENT_DIR];
		process.env[ENV_AGENT_DIR] = agentDir;
		const client = new DaemonCatalogClient(() => {});
		try {
			await expect(client.start()).resolves.toBeUndefined();
			await expect(client.list()).resolves.toEqual([]);
		} finally {
			await client.stop();
			if (previousAgentDir === undefined) delete process.env[ENV_AGENT_DIR];
			else process.env[ENV_AGENT_DIR] = previousAgentDir;
			rmSync(agentDir, { recursive: true, force: true });
		}
	}, 10_000);

	it("serves family and sibling rows from the ledger over IPC", async () => {
		const agentDir = mkdtempSync(join(tmpdir(), "pa-catalog-family-"));
		const previousAgentDir = process.env[ENV_AGENT_DIR];
		process.env[ENV_AGENT_DIR] = agentDir;
		const client = new DaemonCatalogClient(() => {});
		try {
			const sessionsDir = join(agentDir, "sessions");
			mkdirSync(sessionsDir, { recursive: true });
			const parent = SessionManager.create(agentDir, sessionsDir);
			parent.newSession();
			parent.appendSessionInfo("parent");
			parent.flushNow();
			const parentFile = parent.getSessionFile();
			if (!parentFile) throw new Error("Missing parent session file");
			const child = SessionManager.create(agentDir, join(sessionsDir, "artifacts", "sub-11111111"));
			child.newSession({ parentSession: parentFile, rlmDepth: 1 });
			child.appendSessionInfo("worker");
			child.flushNow();
			const childFile = child.getSessionFile();
			if (!childFile) throw new Error("Missing child session file");
			await new RlmSpawnLedger(agentDir, sessionsDir).appendSpawn({
				childId: "sub-11111111",
				parent: parentFile,
				child: childFile,
				depth: 1,
				name: "worker",
			});

			await client.start();
			const family = await client.family(agentDir, sessionsDir);
			expect(family.map((row) => [row.name, row.rlmDepth])).toEqual([
				["parent", 0],
				["worker", 1],
			]);
			expect(family.find((row) => row.name === "worker")?.parentSessionPath).toBe(canonicalSessionPath(parentFile));

			const siblings = await client.siblings(agentDir, childFile, sessionsDir);
			expect(siblings.map((row) => row.name)).toEqual(["worker"]);
		} finally {
			await client.stop();
			if (previousAgentDir === undefined) delete process.env[ENV_AGENT_DIR];
			else process.env[ENV_AGENT_DIR] = previousAgentDir;
			rmSync(agentDir, { recursive: true, force: true });
		}
	}, 10_000);
});
