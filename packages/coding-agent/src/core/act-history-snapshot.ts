import {
	buildSessionHistorySnapshot,
	type HistorySnapshot,
	type HistoryTextLayout,
	layoutHistoryText,
} from "./history-snapshot.js";
import type { SessionEntry } from "./session-manager.js";

/** ActCallerHistory is the caller-side delta rendered for one retained Act. */
export type ActCallerHistory = HistorySnapshot;

/** buildActCallerHistory renders messages since the prior Act at this depth. */
export function buildActCallerHistory(
	entries: readonly SessionEntry[],
	currentToolCallId?: string,
	previousToolCallId?: string,
): ActCallerHistory {
	const previousStart = previousToolCallId ? findToolCall(entries, previousToolCallId) : -1;
	if (previousStart < 0) return { text: "", images: [], messageCount: 0, truncated: false };

	let currentCall = entries.length;
	if (currentToolCallId) {
		const index = findToolCall(entries, currentToolCallId, previousStart + 1);
		if (index >= 0) currentCall = index;
	}
	return buildSessionHistorySnapshot({ entries: entries.slice(previousStart + 1, currentCall) });
}

function findToolCall(entries: readonly SessionEntry[], toolCallId: string, from = 0): number {
	return entries.findIndex(
		(entry, index) =>
			index >= from &&
			entry.type === "message" &&
			entry.message.role === "assistant" &&
			entry.message.content.some((content) => content.type === "toolCall" && content.id === toolCallId),
	);
}

/** Compatibility name for retained-Act callers. */
export type ActHistoryTextLayout = HistoryTextLayout;

/** Compatibility name for retained-Act callers. */
export const layoutActHistoryText = layoutHistoryText;
