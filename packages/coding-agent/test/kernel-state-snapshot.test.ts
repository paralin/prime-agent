import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
	buildRestoreCode,
	buildSnapshotCode,
	DEFAULT_SNAPSHOT_MAX_BYTES,
	DEFAULT_SNAPSHOT_MAX_VARIABLE_BYTES,
	manifestPathIn,
	parseRestoreResult,
	parseSnapshotResult,
	snapshotPathIn,
} from "../src/core/kernel/state-snapshot.js";

const MARKER = "__PRIME_AGENT_KERNEL_STATE__";

describe("kernel state snapshot paths", () => {
	it("places snapshot + manifest inside the session artifact directory", () => {
		const artifactDir = "/home/u/.prime/agent/session-artifacts/abc-123";
		expect(snapshotPathIn(artifactDir)).toBe(join(artifactDir, "kernel-state.dill"));
		expect(manifestPathIn(artifactDir)).toBe(join(artifactDir, "kernel-state.json"));
	});
});
