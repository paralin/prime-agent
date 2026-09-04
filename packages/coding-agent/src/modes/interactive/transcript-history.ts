import type { AgentMessage } from "@earendil-works/pi-agent-core";
import { SCRATCH_HANDOFF_READ_CUSTOM_TYPE } from "../../core/compaction/scratch-handoff.js";
import {
	createBranchSummaryMessage,
	createCompactionSummaryMessage,
	createCustomMessage,
} from "../../core/messages.js";
import type { AgentConnectionSessionEntry, AgentConnectionSessionTreeNode } from "../agent-connection/types.js";

/** Display the selected journal branch without applying model-context compaction. */
export function buildTranscriptHistory(
	tree: readonly AgentConnectionSessionTreeNode[],
	leafId: string | null,
	liveMessages: readonly AgentMessage[],
): AgentMessage[] {
	const entries = new Map<string, AgentConnectionSessionEntry>();
	const stack = [...tree];
	while (stack.length) {
		const node = stack.pop()!;
		entries.set(node.entry.id, node.entry);
		stack.push(...node.children);
	}
	const branch: AgentConnectionSessionEntry[] = [];
	const visited = new Set<string>();
	let entry = leafId ? entries.get(leafId) : undefined;
	while (entry && !visited.has(entry.id)) {
		visited.add(entry.id);
		branch.push(entry);
		entry = entry.parentId ? entries.get(entry.parentId) : undefined;
	}
	branch.reverse();
	if (!branch.length) return [...liveMessages];
	const scratchMessages = new Map<string, string>();
	for (const item of branch) {
		if (item.type !== "compaction") continue;
		const details = item.details as { scratchHandoff?: { version?: number; path?: unknown } } | undefined;
		if (details?.scratchHandoff?.version === 1 && typeof details.scratchHandoff.path === "string") {
			scratchMessages.set(item.firstKeptEntryId, details.scratchHandoff.path);
		}
	}
	const messages: AgentMessage[] = [];
	const persisted = new Set<string>();
	for (const item of branch) {
		if (item.type === "message") {
			persisted.add(`${item.message.role}:${item.message.timestamp}`);
			const path = scratchMessages.get(item.id);
			if (path && item.message.role === "user") {
				const content = item.message.content;
				const text =
					typeof content === "string"
						? content
						: content
								.filter((part) => part.type === "text")
								.map((part) => part.text)
								.join("\n");
				const start = text.indexOf(">\n", text.indexOf("<scratch-handoff-file "));
				const end = text.lastIndexOf("\n</scratch-handoff-file>");
				messages.push(
					createCustomMessage(
						SCRATCH_HANDOFF_READ_CUSTOM_TYPE,
						start >= 0 && end > start ? text.slice(start + 2, end) : text,
						true,
						{ path },
						item.timestamp,
					),
				);
			} else messages.push(item.message);
		} else if (item.type === "custom_message") {
			const message = createCustomMessage(item.customType, item.content, item.display, item.details, item.timestamp);
			persisted.add(`${message.role}:${message.timestamp}`);
			messages.push(message);
		} else if (item.type === "compaction" && item.summary) {
			messages.push(createCompactionSummaryMessage(item.summary, item.tokensBefore, item.timestamp));
		} else if (item.type === "branch_summary") {
			messages.push(createBranchSummaryMessage(item.summary, item.fromId, item.timestamp));
		}
	}
	for (const message of liveMessages) {
		if (
			message.role !== "compactionSummary" &&
			message.role !== "branchSummary" &&
			!persisted.has(`${message.role}:${message.timestamp}`)
		)
			messages.push(message);
	}
	return messages;
}
