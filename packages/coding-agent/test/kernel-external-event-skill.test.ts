import { chmodSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
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
		const watchPublications: Array<Array<Record<string, unknown>>> = [];
		provisioner = new IpythonKernelProvisioner(tempDir, {
			pythonSkills: [bundledExternalEventSkill()],
			hostHandlers: {
				"session.external_event.emit": async (payload) => {
					requests.push(payload);
					receive(payload);
					return { accepted: true, deliveryStatus: "delivered" };
				},
				"session.external_event.watch_update": async (payload) => {
					const jobs = (payload as { jobs?: Array<Record<string, unknown>> }).jobs ?? [];
					watchPublications.push(jobs);
					return { accepted: true, count: jobs.length };
				},
			},
		});

		const manager = await provisioner.ensure();
		const started = await manager.execute(`
job = bash("sleep 0.1; printf capture-complete")
job_id = external_event.watch_bash(job, "video capture", tail_lines=4)
print(job_id)
`);
		expect(started.status, JSON.stringify(started.error)).toBe("ok");
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

		// The registry mirrors itself to the session: once on registration with the
		// job running, and again after completion with the terminal status.
		expect(watchPublications.length).toBeGreaterThanOrEqual(2);
		expect(watchPublications[0]).toMatchObject([{ id: jobId, label: "video capture", status: "running" }]);
		expect(watchPublications.at(-1)).toMatchObject([{ id: jobId, status: "completed" }]);
	});

	it("watches an argv-safe SSH script transport through the live kernel", async () => {
		const fakeBin = join(tempDir, "bin");
		mkdirSync(fakeBin, { recursive: true });
		const fakeSsh = join(fakeBin, "ssh");
		writeFileSync(
			fakeSsh,
			`#!/bin/sh
while [ "$1" != "--" ]; do shift; done
shift
host=$1
shift
command=$1
exec /bin/sh -c "$command"
`,
		);
		chmodSync(fakeSsh, 0o755);
		let receive!: (payload: Record<string, unknown>) => void;
		const received = new Promise<Record<string, unknown>>((resolve) => {
			receive = resolve;
		});
		provisioner = new IpythonKernelProvisioner(tempDir, {
			pythonSkills: [bundledExternalEventSkill()],
			hostHandlers: {
				"session.external_event.emit": async (payload) => {
					receive(payload);
					return { accepted: true, deliveryStatus: "delivered" };
				},
			},
		});

		const manager = await provisioner.ensure();
		const started = await manager.execute(`
import os
os.environ["PATH"] = ${JSON.stringify(fakeBin)} + os.pathsep + os.environ["PATH"]
script = """set -eu
quoted='single and \\"double\\"'
sub=$(printf command-substitution)
cat <<'PAYLOAD'
Unicode 雪 and $(literal)
PAYLOAD
printf '%s\\n' "$REMOTE_TARGET:$sub:$quoted"
"""
remote_job = bash(
    script,
    ssh="core@fake-host",
    cwd=${JSON.stringify(tempDir)},
    env={"REMOTE_TARGET": "desktop value"},
)
remote_job_id = external_event.watch_bash(remote_job, "remote build")
print(remote_job_id)
`);
		expect(started.status, JSON.stringify(started.error)).toBe("ok");
		const jobId = started.stdout.trim();
		const event = await Promise.race([
			received,
			new Promise<never>((_, reject) => setTimeout(() => reject(new Error("SSH event timed out")), 5_000)),
		]);
		expect(event).toMatchObject({
			name: "bash",
			event_id: jobId,
			delivery_policy: "followUp",
		});
		expect(event.text).toContain("SSH: core@fake-host");
		expect(event.text).toContain(`Remote cwd: ${tempDir}`);
		expect(event.text).toContain("Remote env keys: REMOTE_TARGET");
		expect(event.text).toContain("Transport: ssh");
		expect(event.text).toContain("Transport error: no");
		expect(event.text).toContain("Unicode 雪 and $(literal)");
		expect(event.text).toContain('desktop value:command-substitution:single and "double"');
	});
});
