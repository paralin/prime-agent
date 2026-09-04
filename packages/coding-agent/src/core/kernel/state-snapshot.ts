// Locations and result shapes for the kernel's persisted user namespace.
import { join } from "node:path";

export const DEFAULT_SNAPSHOT_MAX_BYTES = 256 * 1024 * 1024;
export const DEFAULT_SNAPSHOT_MAX_VARIABLE_BYTES = 16 * 1024 * 1024;

const KERNEL_STATE_BASENAME = "kernel-state";

export interface SnapshotResult {
	saved: string[];
	skipped: { name: string; reason: string }[];
	pruned?: string[];
	bytes: number;
	path: string;
}

export interface RestoreResult {
	restored: string[];
	failed: { name: string; reason: string }[];
	path: string;
}

export function snapshotPathIn(artifactDir: string): string {
	return join(artifactDir, `${KERNEL_STATE_BASENAME}.dill`);
}

export function manifestPathIn(artifactDir: string): string {
	return join(artifactDir, `${KERNEL_STATE_BASENAME}.json`);
}
