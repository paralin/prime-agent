import { mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { getBundledSkillsDir } from "../src/config.js";
import type { PythonSkillRuntimeInfo } from "../src/core/skills.js";
import { IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

function bundledExternalEventSkill(): PythonSkillRuntimeInfo {
	const packagePath = join(getBundledSkillsDir(), "external-event");
	return {
		name: "external-event",
		importName: "external_event",
		packagePath,
		pyprojectPath: join(packagePath, "pyproject.toml"),
	};
}

describe("external-event skill over the kernel host bridge", { tags: ["kernel-heavy"] }, () => {
	let tempDir: string;
	let provisioner: IpythonKernelProvisioner | undefined;

	beforeEach(() => {
		tempDir = join(tmpdir(), `prime-external-event-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });
	});

	afterEach(async () => {
		await provisioner?.dispose();
		provisioner = undefined;
		rmSync(tempDir, { recursive: true, force: true });
	});

	it("wakes through the host after a retained bash job finishes", async () => {
		const requests: Array<Record<string, unknown>> = [];
		let receive!: (payload: Record<string, unknown>) => void;
		const received = new Promise<Record<string, unknown>>((resolve) => {
			receive = resolve;
		});
		provisioner = new IpythonKernelProvisioner(tempDir, {
			pythonSkills: [bundledExternalEventSkill()],
			hostHandlers: {
				"session.external_event.emit": async (payload) => {
					requests.push(payload);
					receive(payload);
					return { accepted: true, deliveryStatus: "delivered" };
				},
			},
		});

		const manager = await provisioner.ensure();
		const started = await manager.execute(`
job = bash("sleep 0.1; printf capture-complete")
job_id = external_event.watch_bash(job, "video capture", tail_lines=4)
print(job_id)
`);
		expect(started.status).toBe("ok");
		const jobId = started.stdout.trim();
		expect(jobId).toMatch(/^[0-9a-f]{32}$/);

		const event = await Promise.race([
			received,
			new Promise<never>((_, reject) => setTimeout(() => reject(new Error("completion event timed out")), 5_000)),
		]);
		expect(event).toMatchObject({
			type: "session.external_event.emit",
			name: "bash",
			event_id: jobId,
			delivery_policy: "followUp",
		});
		expect(event.text).toContain("video capture");
		expect(event.text).toContain("Exit code: 0");
		expect(event.text).toContain("capture-complete");
		expect(requests).toHaveLength(1);

		const listed = await manager.execute(`
info = external_event.get_job(job_id)
print(info.status, info.exit_code, info.notification_status)
`);
		expect(listed.stdout.trim()).toBe("completed 0 delivered");
		expect(requests).toHaveLength(1);
	});
});
