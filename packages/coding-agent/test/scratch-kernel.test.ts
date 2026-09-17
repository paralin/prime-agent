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

	it("writes and edits only the checkpoint through plain Python calls", async () => {
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
		// The closeout model composes checkpoint text with ordinary Python: keyword
		// arguments, expressions, and multi-line strings all reach the helpers.
		expect(
			await execute(
				'scratch_write(text="""* TODO Active task\\nsecond line""")\nscratch_replace(old="TODO", new="DONE")',
			),
		).toMatchObject({ isError: false });
		expect(readFileSync(path, "utf8")).toBe("* DONE Active task\nsecond line");
		expect((await execute('scratch_write("* TODO " + "recomposed\\n")')).content).toEqual([
			expect.objectContaining({ type: "text", text: expect.stringContaining("Saved handoff checkpoint") }),
		]);
		expect(readFileSync(path, "utf8")).toBe("* TODO recomposed\n");
		for (const code of [
			'scratch_write("   ")',
			'scratch_replace("absent", "bad")',
			'scratch_read("another.org")',
			'scratch_write("x", "extra")',
		]) {
			expect(await execute(code), code).toMatchObject({ isError: true });
			expect(readFileSync(path, "utf8")).toBe("* TODO recomposed\n");
		}
	}, 30_000);

	it("restores the same working kernel after closeout success", async () => {
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
			fauxAssistantMessage("Checkpoint saved."),
		]);
		await harness.session.prompt("Work on the active task");
		await harness.session.compact();
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
	}, 30_000);

	it.each(["failure", "cancelled"])(
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
			harness.setResponses([
				fauxAssistantMessage("Started task."),
				(context) => {
					expect(context.tools?.map((tool) => tool.name)).toEqual(["ipython"]);
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
					return fauxAssistantMessage("", { stopReason: "error", errorMessage: "fixture failure" });
				},
			]);
			await harness.session.prompt("Work on the active task");
			await expect(harness.session.compact()).rejects.toThrow(
				outcome === "cancelled" ? "Compaction cancelled" : "fixture failure",
			);
			const restored = harness.session.agent.state.tools.find((tool) => tool.name === "ipython")!;
			expect(restored.description).toBe(original.description);
			expect(working.manager).toBe(manager);
			const resumed = await restored.execute("resume", {
				code: "assert id(sentinel) == sentinel_id\nprint('working state retained')",
			});
			expect(resumed).toMatchObject({ isError: false });
		},
		30_000,
	);
});
