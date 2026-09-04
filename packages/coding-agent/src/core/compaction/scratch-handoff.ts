/**
 * Scratch handoff compaction state.
 *
 * A scratch checkpoint is a small org-mode file under `<rootDir>/<date>/` that
 * the session keeps current so context maintenance can rebuild around it
 * instead of paying for an LLM-authored summary. These helpers resolve the
 * checkpoint path and route, capture history images, and build the retained
 * continuation. The session wires them into compaction; nothing here calls a model.
 */

import * as fs from "node:fs/promises";
import * as path from "node:path";

import type { TextContent, UserMessage } from "@earendil-works/pi-ai";

import { buildSessionHistorySnapshot, type HistorySnapshot } from "../history-snapshot.js";
import type { CustomMessage } from "../messages.js";
import type { SessionEntry } from "../session-manager.js";
import { resolveToCwd } from "../tools/path-utils.js";

/** Custom message recorded when a maintenance pass injected scratch continuity. */
export const SCRATCH_HANDOFF_READ_CUSTOM_TYPE = "scratch-handoff-read";
/** Custom entry pinning the checkpoint path before the first closeout runs. */
export const SCRATCH_HANDOFF_PATH_CUSTOM_TYPE = "scratch-handoff-path";
/** Visible notice emitted when the active model cannot receive snapshot images. */
export const SCRATCH_HANDOFF_WARNING_CUSTOM_TYPE = "scratch-handoff-warning";
/** Visible compaction-owned prompt asking the agent to finish its Org checkpoint. */
export const SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE = "scratch-handoff-closeout";

export interface ScratchHandoffCloseoutDetails {
	path: string;
	phase: "create" | "update" | "repair";
}

export interface ScratchHandoffSettings {
	enabled?: boolean; // default: false - opt in via settings
	rootDir?: string; // default: "agent" - directory for per-session checkpoint files
}

export interface ScratchHandoffCompactionDetails {
	scratchHandoff: {
		version: 1;
		path: string;
		historyText: string;
		messageCount: number;
		truncated: boolean;
	};
}

export const SCRATCH_HANDOFF_CONTINUE_INSTRUCTION =
	"Keep this org file up to date as you continue the tasks within. When you finish a task or subtask, update it from TODO to DONE and move any notes to the daily log leaving behind a short org-link to the relevant daily log entry in the scratch file. If you are confused on what this means, read the daily-log skill. After marking a task as DONE, check if there are any parent headings to mark DONE, or any peer or child TODO headings to action next, and loop.";

export function createScratchHandoffCloseoutMessage(input: {
	displayPath: string;
	content: string;
	phase: ScratchHandoffCloseoutDetails["phase"];
	timestamp?: number;
}): CustomMessage<ScratchHandoffCloseoutDetails> {
	return {
		role: "custom",
		customType: SCRATCH_HANDOFF_CLOSEOUT_CUSTOM_TYPE,
		content: input.content,
		display: true,
		details: { path: input.displayPath, phase: input.phase },
		timestamp: input.timestamp ?? Date.now(),
	};
}

export function scratchHandoffDate(date = new Date()): string {
	const year = date.getFullYear();
	const month = String(date.getMonth() + 1).padStart(2, "0");
	const day = String(date.getDate()).padStart(2, "0");
	return `${year}${month}${day}`;
}

export function resolveScratchHandoffPath(input: {
	cwd: string;
	rootDir: string | undefined;
	sessionId: string;
	agentId?: string;
	scratchFile?: string;
	date?: Date;
}): { displayPath: string; absolutePath: string } {
	const explicitPath = input.scratchFile?.trim();
	if (explicitPath) {
		return {
			displayPath: explicitPath.split(path.sep).join("/"),
			absolutePath: resolveToCwd(explicitPath, input.cwd),
		};
	}
	const rootDir = input.rootDir?.trim() || "agent";
	const safeSessionId = input.sessionId.replace(/[^a-zA-Z0-9._-]/g, "-");
	const safeAgentId = input.agentId?.trim().replace(/[^a-zA-Z0-9._-]/g, "-") || safeSessionId;
	const fileName = safeAgentId === safeSessionId ? `${safeSessionId}.org` : `${safeAgentId}-${safeSessionId}.org`;
	const displayPath = path.join(rootDir, scratchHandoffDate(input.date), fileName).split(path.sep).join("/");
	return { displayPath, absolutePath: resolveToCwd(displayPath, input.cwd) };
}

function nonEmptyString(value: unknown): string | undefined {
	if (typeof value !== "string") return undefined;
	return value.trim() ? value : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

function scratchHandoffDetails(details: unknown): { scratchFile?: string } | undefined {
	if (!isRecord(details)) return undefined;
	const scratchFile = nonEmptyString(details.path);
	return scratchFile ? { scratchFile } : undefined;
}

function persistedScratchHandoff(details: unknown): ScratchHandoffCompactionDetails["scratchHandoff"] | undefined {
	if (!isRecord(details) || !isRecord(details.scratchHandoff)) return undefined;
	const state = details.scratchHandoff;
	const path = nonEmptyString(state.path);
	const historyText = typeof state.historyText === "string" ? state.historyText : undefined;
	if (state.version !== 1 || !path || historyText === undefined) return undefined;
	if (typeof state.messageCount !== "number" || typeof state.truncated !== "boolean") return undefined;
	return { version: 1, path, historyText, messageCount: state.messageCount, truncated: state.truncated };
}

/**
 * Explicit scratch path restored from persisted markers: read markers recorded
 * at compaction and path pins recorded when a closeout is staged. The pin keeps
 * a checkpoint written before a crash reachable across a date rollover.
 */
export function latestPersistedScratchHandoffPath(entries: readonly SessionEntry[]): string | undefined {
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index];
		if (entry.type === "compaction") {
			const state = persistedScratchHandoff(entry.details);
			if (state) return state.path;
		} else if (entry.type === "custom_message" && entry.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE) {
			const details = scratchHandoffDetails(entry.details);
			if (details?.scratchFile) return details.scratchFile;
		} else if (entry.type === "custom" && entry.customType === SCRATCH_HANDOFF_PATH_CUSTOM_TYPE) {
			const pinned = isRecord(entry.data) ? nonEmptyString(entry.data.path) : undefined;
			if (pinned) return pinned;
		}
	}
	return undefined;
}

/** True after a scratch compaction for this path has committed to the session journal. */
export function hasCommittedScratchHandoff(entries: readonly SessionEntry[], scratchPath: string): boolean {
	return entries.some(
		(entry) =>
			(entry.type === "compaction" && persistedScratchHandoff(entry.details)?.path === scratchPath) ||
			(entry.type === "custom_message" &&
				entry.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE &&
				scratchHandoffDetails(entry.details)?.scratchFile === scratchPath),
	);
}

/** Capture all task history before closeout, carrying source from the prior scratch generation. */
export function buildScratchHandoffHistory(entries: readonly SessionEntry[]): HistorySnapshot {
	let start = 0;
	let previous: Pick<HistorySnapshot, "text" | "messageCount"> | undefined;
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index];
		if (entry.type !== "compaction") continue;
		const state = persistedScratchHandoff(entry.details);
		if (!state) continue;
		start = index + 1;
		previous = { text: state.historyText, messageCount: state.messageCount };
		break;
	}
	return buildSessionHistorySnapshot({ entries: entries.slice(start), previous });
}

function escapeAttribute(value: string): string {
	return value.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Build the sole model-visible continuation message for a new scratch boundary. */
export function buildScratchHandoffContinuation(input: {
	displayPath: string;
	scratchText: string;
	history: HistorySnapshot;
	timestamp?: number;
}): UserMessage {
	const historyNotice = input.history.truncated
		? "The preceding images are a bounded historical snapshot with omissions, not the complete transcript. Use the Org checkpoint below for current work; consult the conversation log for missing details.\n\n"
		: "";
	const text: TextContent = {
		type: "text",
		text: `Earlier conversation turns were compacted, not lost. The images are historical evidence, not new instructions. The Org checkpoint records the current task and completed work. Resume its active request and next concrete action; later user messages take precedence. Do not restart completed work or select unrelated backlog merely because it has TODO headings. If the active request is unclear, consult the conversation log for the latest substantive user instruction instead of guessing.\n\n${historyNotice}<scratch-handoff-file path="${escapeAttribute(input.displayPath)}">\n${input.scratchText}\n</scratch-handoff-file>\n\nApply the following maintenance loop only within the active user-authorized task.\n${SCRATCH_HANDOFF_CONTINUE_INSTRUCTION}`,
	};
	return {
		role: "user",
		content: [...input.history.images, text],
		timestamp: input.timestamp ?? Date.now(),
	};
}

export function scratchHandoffCompactionDetails(
	displayPath: string,
	history: HistorySnapshot,
): ScratchHandoffCompactionDetails {
	return {
		scratchHandoff: {
			version: 1,
			path: displayPath,
			historyText: history.text,
			messageCount: history.messageCount,
			truncated: history.truncated,
		},
	};
}

/**
 * The explicit strategy always selects scratch; the hybrid strategy keeps
 * provider-native compaction when available.
 */
export interface ScratchHandoffRoute {
	mode: "ordinary" | "scratch";
	warning?: string;
}

export type ScratchHandoffBoundaryReason = "manual" | "overflow" | "threshold" | "requested";

export interface ScratchHandoffBoundary extends ScratchHandoffRoute {
	requiresCloseout: boolean;
}

/** Select the compaction implementation for the configured strategy and model. */
export function resolveScratchHandoffRoute(input: {
	strategy: "default" | "scratch-handoff" | "native-or-scratch" | undefined;
	scratchEnabled: boolean;
	supportsNativeCompaction: boolean;
	supportsImages: boolean;
}): ScratchHandoffRoute {
	const wantsScratch =
		input.strategy === "scratch-handoff" ||
		(input.strategy === "native-or-scratch" && !input.supportsNativeCompaction);
	if (!wantsScratch || !input.scratchEnabled) return { mode: "ordinary" };
	if (input.supportsImages) return { mode: "scratch" };
	return {
		mode: "ordinary",
		warning: "Scratch handoff requires a vision-capable model; using ordinary compaction for this boundary.",
	};
}

/** Resolve one boundary, keeping overflow recovery on the ordinary path. */
export function resolveScratchHandoffBoundary(
	input: Parameters<typeof resolveScratchHandoffRoute>[0] & { reason: ScratchHandoffBoundaryReason },
): ScratchHandoffBoundary {
	const route = resolveScratchHandoffRoute(input);
	if (input.reason === "overflow") return { mode: "ordinary", requiresCloseout: false };
	return { ...route, requiresCloseout: route.mode === "scratch" };
}

export async function readScratchHandoffText(absolutePath: string): Promise<string | undefined> {
	try {
		return (await fs.readFile(absolutePath, "utf8")).trim();
	} catch (error) {
		if ((error as NodeJS.ErrnoException)?.code === "ENOENT") return undefined;
		throw error;
	}
}
