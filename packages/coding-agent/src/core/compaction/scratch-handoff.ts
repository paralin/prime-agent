/**
 * Scratch handoff continuity state.
 *
 * A scratch checkpoint is a small org-mode file under `<rootDir>/<date>/` that
 * the session keeps current so context maintenance can rebuild around it
 * instead of paying for an LLM-authored summary. These helpers resolve the
 * checkpoint path, classify how well the document covers recent work, size the
 * inline delta that rides past the compaction boundary, and build the resume
 * payload. The session wires them into compaction; nothing here calls a model.
 */

import * as fs from "node:fs/promises";
import * as path from "node:path";

import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { Message } from "@earendil-works/pi-ai";

import { convertToLlm, createCustomMessage } from "../messages.js";
import { renderScratchHandoffResumeMessage } from "../prompts/scratch-handoff.js";
import type { SessionEntry } from "../session-manager.js";
import { resolveToCwd } from "../tools/path-utils.js";
import { serializeConversation } from "./utils.js";

/** Custom message recorded when a maintenance pass injected scratch continuity. */
export const SCRATCH_HANDOFF_READ_CUSTOM_TYPE = "scratch-handoff-read";
/** Custom entry recorded when a closeout turn changed the scratch document. */
export const SCRATCH_HANDOFF_WRITE_CUSTOM_TYPE = "scratch-handoff-write";

export interface ScratchHandoffSettings {
	enabled?: boolean; // default: false - opt in via settings
	rootDir?: string; // default: "agent" - directory for per-session checkpoint files
}

/** Token estimate for plain text using the same chars/4 heuristic as estimateTokens. */
function countTextTokens(text: string): number {
	return Math.ceil(text.length / 4);
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

interface ScratchTodoSubtree {
	objective: string;
	nextAction: string;
}

function fieldValue(lines: readonly string[], label: string): string {
	const prefix = `- ${label}:`;
	const index = lines.findIndex((line) => line.startsWith(prefix));
	if (index < 0) return "";
	const values = [lines[index].slice(prefix.length).trim()];
	for (let cursor = index + 1; cursor < lines.length; cursor++) {
		const line = lines[cursor];
		if (/^-\s+\S[^:]*:/.test(line)) break;
		if (/^\s+(?:[-+*]|\d+\.)\s+\S/.test(line)) values.push(line.trim());
		else if (line.trim()) break;
	}
	return values.filter(Boolean).join("\n");
}

/** Parse one unambiguous root TODO and its direct field body. */
function activeScratchTodo(text: string): ScratchTodoSubtree | undefined {
	const lines = text.split(/\r?\n/);
	const roots: number[] = [];
	for (let index = 0; index < lines.length; index++) {
		if (/^\*\s+TODO\s+\S/.test(lines[index])) roots.push(index);
	}
	if (roots.length !== 1) return undefined;
	const start = roots[0] + 1;
	let end = lines.length;
	for (let index = start; index < lines.length; index++) {
		if (/^\*+\s+(?:TODO|DONE)\s+\S/.test(lines[index])) {
			end = index;
			break;
		}
	}
	const body = lines.slice(start, end);
	return {
		objective: fieldValue(body, "Objective"),
		nextAction: fieldValue(body, "Next action"),
	};
}

/**
 * Continuity state of the scratch document for one maintenance pass.
 *
 * - `verified`: resumable content that already covers every message in the delta
 *   window, so the rebuild needs no extra material.
 * - `stale`: resumable content with work recorded after the last scratch write.
 *   The attached delta carries that work; this is the ordinary state whenever
 *   context pressure arrives before a closeout turn.
 * - `unusable`: the document lacks the objective, open TODO, or next action an
 *   autonomous resume needs, so the model must rebuild it.
 */
export type ScratchContinuityState = "verified" | "stale" | "unusable";

/** True when the active root TODO contains resumable content. */
export function scratchHandoffHasContent(text: string): boolean {
	const todo = activeScratchTodo(text);
	return todo !== undefined && (todo.objective.length > 0 || todo.nextAction.length > 0);
}

/** True when one active root TODO has its own objective and next action. */
export function scratchHandoffIsComplete(text: string): boolean {
	const todo = activeScratchTodo(text);
	return todo !== undefined && todo.objective.length > 0 && todo.nextAction.length > 0;
}

/** Classify scratch continuity from document content and recorded write state. */
export function resolveScratchContinuityState(input: {
	scratchText: string;
	/** A closeout turn wrote the document during this maintenance episode. */
	closeoutWriteCompleted: boolean;
	/** The current branch records at least one write to this scratch path. */
	hasRecordedWrite: boolean;
	/** Session work landed after the newest recorded write. */
	hasDelta: boolean;
}): ScratchContinuityState {
	if (!scratchHandoffIsComplete(input.scratchText)) return "unusable";
	if (input.closeoutWriteCompleted) return "verified";
	return input.hasRecordedWrite && !input.hasDelta ? "verified" : "stale";
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

/** Explicit scratch path restored from the latest persisted read marker. */
export function latestPersistedScratchHandoffPath(entries: readonly SessionEntry[]): string | undefined {
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index];
		if (entry.type !== "custom_message" || entry.customType !== SCRATCH_HANDOFF_READ_CUSTOM_TYPE) continue;
		const details = scratchHandoffDetails(entry.details);
		if (details?.scratchFile) return details.scratchFile;
	}
	return undefined;
}

/**
 * Maximum scratch-body prefix injected after a scratch compaction. Detailed
 * history remains readable from disk.
 */
export const SCRATCH_HANDOFF_BODY_MAX_TOKENS = 2_048;

/** Smallest inline delta the recent-context budget ever allows. */
export const SCRATCH_HANDOFF_RECENT_CONTEXT_MIN_TOKENS = 2_048;

/** Share of the context window an inline delta may consume. */
export const SCRATCH_HANDOFF_RECENT_CONTEXT_WINDOW_FRACTION = 0.1;

export interface ScratchHandoffBodyPreview {
	text: string;
	truncated: boolean;
}

/** Keep a token-safe beginning of the scratch body. */
export function scratchHandoffBodyPreview(
	text: string,
	maxTokens = SCRATCH_HANDOFF_BODY_MAX_TOKENS,
): ScratchHandoffBodyPreview {
	if (countTextTokens(text) <= maxTokens) return { text, truncated: false };
	const lines = text.split(/(?<=\n)/);
	let kept = 0;
	let tokens = 0;
	for (; kept < lines.length; kept++) {
		const lineTokens = countTextTokens(lines[kept]);
		if (tokens + lineTokens > maxTokens) break;
		tokens += lineTokens;
	}
	if (kept > 0) return { text: lines.slice(0, kept).join("").trimEnd(), truncated: true };

	const codepoints = Array.from(text);
	let end = 0;
	tokens = 0;
	while (end < codepoints.length) {
		const next = countTextTokens(codepoints[end]);
		if (tokens + next > maxTokens) break;
		tokens += next;
		end++;
	}
	return { text: codepoints.slice(0, end).join(""), truncated: true };
}

/**
 * Size the inline delta carried past a handoff. Everything in the rebuilt
 * context is a new prefix paid at full price, so an unbounded delta re-buys the
 * context the handoff exists to release.
 */
export function scratchHandoffRecentContextBudget(contextWindow: number): number {
	if (!Number.isFinite(contextWindow) || contextWindow <= 0) return SCRATCH_HANDOFF_RECENT_CONTEXT_MIN_TOKENS;
	return Math.max(
		SCRATCH_HANDOFF_RECENT_CONTEXT_MIN_TOKENS,
		Math.floor(contextWindow * SCRATCH_HANDOFF_RECENT_CONTEXT_WINDOW_FRACTION),
	);
}

/**
 * Index of the first entry the latest compaction kept. Entries before it left
 * the model context at that boundary, so the delta window must never reach back
 * across it even when no scratch write has been recorded since.
 */
function latestCompactionBoundaryIndex(entries: readonly SessionEntry[]): number {
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index];
		if (entry.type !== "compaction") continue;
		const keptIndex = entries.findIndex((candidate) => candidate.id === entry.firstKeptEntryId);
		return keptIndex >= 0 ? keptIndex : index + 1;
	}
	return 0;
}

/** First entry of the work not yet represented in the scratch document. */
function scratchHandoffDeltaStartIndex(entries: readonly SessionEntry[], scratchPath?: string): number {
	let start = 0;
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index];
		if (entry.type !== "custom" || entry.customType !== SCRATCH_HANDOFF_WRITE_CUSTOM_TYPE) continue;
		if (scratchPath && (!isRecord(entry.data) || entry.data.path !== scratchPath)) continue;
		start = index + 1;
		break;
	}
	return Math.max(start, latestCompactionBoundaryIndex(entries));
}

/** Work recorded after the scratch document was last written. */
export interface ScratchHandoffDelta {
	text: string;
	bounded: string;
}

/** Keep the newest tail of `text` that fits beside `prefix` in the token budget. */
function tailWithinTokenBudget(text: string, maxTokens: number, prefix: string): string {
	if (countTextTokens(`${prefix}${text}`) <= maxTokens) return `${prefix}${text}`;
	const codepoints = Array.from(text);
	let low = 0;
	let high = codepoints.length;
	while (low < high) {
		const middle = Math.ceil((low + high) / 2);
		if (countTextTokens(`${prefix}${codepoints.slice(codepoints.length - middle).join("")}`) <= maxTokens) {
			low = middle;
		} else {
			high = middle - 1;
		}
	}
	return `${prefix}${codepoints.slice(codepoints.length - low).join("")}`;
}

/** Keep the newest complete messages whose serialized conversation fits. */
function trimDeltaToBudget(messages: Message[], maxTokens: number): { kept: Message[]; dropped: number } {
	if (!Number.isFinite(maxTokens) || maxTokens <= 0) return { kept: messages, dropped: 0 };
	let start = messages.length;
	for (let index = messages.length - 1; index >= 0; index--) {
		if (countTextTokens(serializeConversation(messages.slice(index))) > maxTokens) break;
		start = index;
	}
	return { kept: messages.slice(start), dropped: start };
}

function entryMessage(entry: SessionEntry): AgentMessage | undefined {
	if (entry.type === "message") return entry.message;
	if (entry.type !== "custom_message") return undefined;
	if (entry.customType === SCRATCH_HANDOFF_READ_CUSTOM_TYPE) return undefined;
	return createCustomMessage(entry.customType, entry.content, entry.display, entry.details, entry.timestamp);
}

/** Work recorded after the last scratch write, trimmed to an inline budget. */
export function buildScratchHandoffRecentContext(input: {
	entries: readonly SessionEntry[];
	pendingMessages?: readonly AgentMessage[];
	scratchPath?: string;
	convertToLlm?: (messages: AgentMessage[]) => Message[];
	/** Token ceiling for {@link ScratchHandoffDelta.bounded}; unbounded when omitted. */
	maxTokens?: number;
}): ScratchHandoffDelta | undefined {
	const pendingMessages = input.pendingMessages ?? [];
	const toLlm = input.convertToLlm ?? convertToLlm;
	const messages = toLlm([
		...input.entries
			.slice(scratchHandoffDeltaStartIndex(input.entries, input.scratchPath))
			.map(entryMessage)
			.filter((message): message is AgentMessage => message !== undefined),
		...pendingMessages,
	]);
	const text = serializeConversation(messages).trim();
	if (text.length === 0) return undefined;
	const maxTokens = input.maxTokens ?? Number.POSITIVE_INFINITY;
	let { kept, dropped } = trimDeltaToBudget(messages, maxTokens);
	if (dropped === 0) return { text, bounded: text };

	let serializedKept = serializeConversation(kept).trim();
	let prefix = "";
	while (true) {
		const omitted =
			kept.length === 0
				? `${dropped} message${dropped === 1 ? "" : "s"}; newest message exceeded the inline budget`
				: `${dropped} older message${dropped === 1 ? "" : "s"}`;
		prefix = `[Older session context dropped: ${omitted} omitted. Re-derive missing detail from workspace or linked artifacts; never assume it.]\n\n`;
		if (kept.length === 0 || countTextTokens(`${prefix}${serializedKept}`) <= maxTokens) break;
		kept = kept.slice(1);
		dropped++;
		serializedKept = serializeConversation(kept).trim();
	}
	let bounded: string;
	if (kept.length > 0) {
		bounded = `${prefix}${serializedKept}`;
	} else {
		bounded = tailWithinTokenBudget(text, maxTokens, prefix);
	}
	return { text, bounded };
}

/**
 * True when compaction should rebuild from a scratch checkpoint instead of an
 * LLM-authored summary: the native-or-scratch strategy routes provider-native
 * compaction to models whose API supports it and hands every other model a
 * scratch-checkpoint rebuild.
 */
export function shouldUseScratchHandoffFallback(input: {
	strategy: "default" | "native-or-scratch" | undefined;
	scratchEnabled: boolean;
	supportsNativeCompaction: boolean;
}): boolean {
	return input.strategy === "native-or-scratch" && input.scratchEnabled && !input.supportsNativeCompaction;
}

export async function readScratchHandoffText(absolutePath: string): Promise<string | undefined> {
	try {
		return (await fs.readFile(absolutePath, "utf8")).trim();
	} catch (error) {
		if ((error as NodeJS.ErrnoException)?.code === "ENOENT") return undefined;
		throw error;
	}
}

/** Resume payload injected after a scratch-anchored compaction. */
export function buildScratchHandoffResumeMessage(input: {
	displayPath: string;
	scratchText: string | undefined;
	recentContext?: ScratchHandoffDelta;
}): string {
	const preview = scratchHandoffBodyPreview(input.scratchText ?? "");
	return renderScratchHandoffResumeMessage({
		displayPath: input.displayPath,
		exists: input.scratchText !== undefined,
		scratchText: preview.text,
		scratchTruncated: preview.truncated,
		recentContextText: input.recentContext?.bounded,
	});
}
