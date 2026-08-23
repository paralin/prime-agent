import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const childPath = resolve(__dirname, "fixtures/daemon-worker-client-timeout-child.ts");
const tsxPath = resolve(__dirname, "../../../node_modules/tsx/dist/cli.mjs");

describe("DaemonWorkerClient timeout under supervisor-equivalent policy", () => {
	it("keeps the process alive and the client usable after a backpressured timeout", async () => {
		const child = spawn(process.execPath, [tsxPath, childPath], { stdio: ["ignore", "pipe", "pipe"] });
		let stdout = "";
		child.stdout?.on("data", (chunk: Buffer) => {
			stdout += String(chunk);
		});
		let stderr = "";
		child.stderr?.on("data", (chunk: Buffer) => {
			stderr += String(chunk);
		});
		const exitCode = await new Promise<number | null>((resolveExit) => {
			const timer = setTimeout(() => resolveExit(null), 15_000);
			child.on("exit", (code) => {
				clearTimeout(timer);
				resolveExit(code);
			});
		});
		if (exitCode === null) child.kill("SIGKILL");

		expect(stdout).toContain("PHASE1_TIMEOUT=true");
		expect(stdout).toContain("PHASE2_OK=true");
		expect(stdout).toContain("ALIVE");
		expect(stdout).not.toContain("CHILD_UNHANDLED_REJECTION");
		expect(stderr).not.toContain("UnhandledPromiseRejection");
	}, 20_000);
});
