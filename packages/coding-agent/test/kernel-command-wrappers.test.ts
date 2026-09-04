import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

describe("IPython argv command wrappers", { tags: ["kernel-heavy"] }, () => {
	let tempDir: string;
	let provisioner: IpythonKernelProvisioner | undefined;

	beforeEach(() => {
		tempDir = join(tmpdir(), `prime-agent-command-wrappers-${process.pid}-${Date.now()}`);
		mkdirSync(tempDir, { recursive: true });
	});

	afterEach(async () => {
		await provisioner?.dispose();
		rmSync(tempDir, { recursive: true, force: true });
	});

	it("searches and synchronizes literal paths through live BashHandles", async () => {
		const source = join(tempDir, "source with spaces");
		const destination = join(tempDir, "destination with spaces");
		mkdirSync(source, { recursive: true });
		writeFileSync(join(source, "literal.txt"), "literal $HOME * ' quote\n");
		provisioner = new IpythonKernelProvisioner(tempDir);
		const manager = await provisioner.ensure();
		const result = await manager.execute(`
from pathlib import Path
search_job = rg("literal $HOME * ' quote", ${JSON.stringify(source)}, options=("-F", "-n"))
search_kind = type(search_job).__name__
search_result = await search_job
sync_job = rsync(${JSON.stringify(`${source}/`)}, ${JSON.stringify(destination)})
sync_kind = type(sync_job).__name__
sync_result = await sync_job
print(search_kind, search_result.exit_code, sync_kind, sync_result.exit_code)
print(search_result.output)
print(Path(${JSON.stringify(join(destination, "literal.txt"))}).read_text())
`);

		expect(result.status, JSON.stringify(result.error)).toBe("ok");
		expect(result.stdout).toContain("BashHandle 0 BashHandle 0");
		expect(result.stdout).toContain("literal.txt:1:literal $HOME * ' quote");
		expect(result.stdout).toContain("literal $HOME * ' quote");
	}, 60_000);
});
