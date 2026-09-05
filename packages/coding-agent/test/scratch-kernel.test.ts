import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { resolve } from "node:path";

import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { resolveScratchHandoffPath } from "../src/core/compaction/scratch-handoff.js";
import { ScratchKernel } from "../src/core/compaction/scratch-kernel.js";
import * as kernelBootstrap from "../src/core/kernel/bootstrap.js";
import { createIpythonTool, IpythonKernelProvisioner } from "../src/core/tools/ipython.js";
import { createHarness, type Harness } from "./suite/harness.js";

const python = [
	process.env.PRIME_AGENT_KERNEL_PYTHON,
	resolve(__dirname, "../../../prime-agent-runtime/.venv/bin/python"),
	resolve(homedir(), ".prime/agent/kernel-venv/bin/python"),
].find(
	(candidate) =>
		candidate && existsSync(candidate) && spawnSync(candidate, ["-c", "import rlm.repl, dill"]).status === 0,
);

describe.skipIf(!python)("scratch closeout kernel (real runtime)", () => {
	let harness: Harness | undefined;
	let scratch: ScratchKernel | undefined;
	let working: IpythonKernelProvisioner | undefined;

	afterEach(async () => {
		await scratch?.dispose();
		await working?.dispose({ snapshot: false });
		await harness?.session.disposeAsync();
		harness?.cleanup();
		vi.restoreAllMocks();
	});

	function useRuntime(): void {
		// Provisioning packages has its own suite; execute the installed fixture runtime without installing anything.
		vi.spyOn(kernelBootstrap, "ensureKernelPython").mockResolvedValue(python!);
	}

	it("writes and edits only the checkpoint, rejecting whole invalid cells before executing them", async () => {
		useRuntime();
		harness = await createHarness({ tools: [] });
		const path = resolve(harness.tempDir, "scratch/checkpoint.org");
		scratch = new ScratchKernel(harness.tempDir, path);
		expect(existsSync(path)).toBe(false);
		const execute = (code: string) => scratch!.tool.execute("scratch", { code });
		expect(await execute('scratch_write("* TODO Active task\\n")')).toMatchObject({ isError: false });
		expect((await execute('scratch_replace("TODO", "DONE")\nscratch_read()')).content).toEqual([
			expect.objectContaining({ type: "text", text: expect.stringContaining("* DONE Active task") }),
		]);
		for (const code of [
			"import os",
			'await bash("touch forbidden")',
			'rlm.run("continue working")',
			'open("forbidden", "w").write("bad")',
			'scratch_write(__import__("os").getcwd())',
			'scratch_write("overwrite"); import os',
			"scratch_read.__globals__",
			'scratch_read("another.org")',
			'scratch_replace("absent", "bad")',
		]) {
			expect(await execute(code), code).toMatchObject({ isError: true });
			expect(readFileSync(path, "utf8")).toBe("* DONE Active task\n");
		}
		expect(existsSync(resolve(harness.tempDir, "forbidden"))).toBe(false);
	}, 30_000);

	it.each(["success", "failure", "cancelled"])(
		"restores the same working kernel after closeout %s",
		async (outcome) => {
			useRuntime();
			const tempDir = mkdtempSync(resolve(tmpdir(), "prime-scratch-kernel-"));
			working = new IpythonKernelProvisioner(tempDir);
			const original = createIpythonTool(tempDir, { provisioner: working });
			harness = await createHarness({
				tempDir,
				tools: [original],
				settings: {
					compaction: { enabled: true, strategy: "scratch-handoff" },
					scratchHandoff: { enabled: true },
				},
			});
			await original.execute("setup", { code: "sentinel = object()\nsentinel_id = id(sentinel)" });
			const manager = working.manager;
			const path = resolveScratchHandoffPath({
				cwd: harness.tempDir,
				rootDir: undefined,
				sessionId: harness.session.sessionId,
			}).absolutePath;
			harness.setResponses([
				fauxAssistantMessage("Started task."),
				(context) => {
					expect(context.tools?.map((tool) => tool.name)).toEqual(["ipython"]);
					expect(JSON.stringify(context.messages.at(-1))).toContain("separate scratch-compaction kernel");
					return fauxAssistantMessage(
						[
							{
								type: "toolCall",
								id: "closeout",
								name: "ipython",
								arguments: { code: 'scratch_write("* TODO Active task\\nContinue the requested work.")' },
							},
						],
						{ stopReason: "toolUse" },
					);
				},
				() => {
					if (outcome === "cancelled") {
						harness!.session.abortCompaction();
						return fauxAssistantMessage("", { stopReason: "aborted" });
					}
					return fauxAssistantMessage(
						outcome === "success" ? "Checkpoint saved." : "",
						outcome === "failure" ? { stopReason: "error", errorMessage: "fixture failure" } : {},
					);
				},
			]);
			await harness.session.prompt("Work on the active task");
			if (outcome === "success") await harness.session.compact();
			else
				await expect(harness.session.compact()).rejects.toThrow(
					outcome === "failure" ? "fixture failure" : "Compaction cancelled",
				);
			expect(readFileSync(path, "utf8")).toContain("* TODO Active task");
			const restored = harness.session.agent.state.tools.find((tool) => tool.name === "ipython")!;
			expect(restored.description).toBe(original.description);
			expect(working.manager).toBe(manager);
			const resumed = await restored.execute("resume", {
				code: "assert id(sentinel) == sentinel_id\nprint('working state retained')",
			});
			expect(resumed).toMatchObject({ isError: false });
			expect(resumed.content).toEqual([
				expect.objectContaining({ text: expect.stringContaining("working state retained") }),
			]);
		},
		30_000,
	);
});
