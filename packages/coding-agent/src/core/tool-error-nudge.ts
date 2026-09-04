import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { CustomMessage } from "./messages.js";
import type { IpythonToolDetails } from "./tools/ipython.js";

export const TOOL_ERROR_NUDGE_THRESHOLD = 3;
export const TOOL_ERROR_NUDGE_CUSTOM_TYPE = "tool_error_nudge";
export const TOOL_ERROR_NUDGE_PREVIEW_LABEL = "Tool error notice";

export const BASH_ERROR_NUDGE_PROMPT =
	"Your bash calls are malformed or have syntax errors or otherwise are returning errors. Carefully take a step back and think through the syntax and what you're trying to do before trying again.";

export const PYTHON_SYNTAX_NUDGE_PROMPT =
	"Your Python has invalid syntax. Carefully take a step back and think through the syntax and what you're trying to do before trying again.";

const BASH_CALL_RE = /\bbash\s*\(/;
const PYTHON_SYNTAX_ENAMES = new Set(["SyntaxError", "IndentationError", "TabError"]);
const EXIT_CODE_RE = /\bexit_code=(-?\d+)\b/;
const TRANSPORT_ERROR_RE = /\btransport_error=True\b/;

export type ToolErrorNudgeKind = "bash" | "python-syntax";

export interface ToolErrorNudge {
	kind: ToolErrorNudgeKind;
	prompt: string;
}

export interface ToolErrorNudgeDetails {
	kind: ToolErrorNudgeKind;
}

function ipythonCode(args: unknown): string {
	if (!args || typeof args !== "object" || !("code" in args) || typeof args.code !== "string") {
		return "";
	}
	return args.code;
}

function ipythonDetails(details: unknown): IpythonToolDetails | undefined {
	if (!details || typeof details !== "object") return undefined;
	return details as IpythonToolDetails;
}

function toolResultText(details: unknown, content: unknown): string {
	const typed = ipythonDetails(details);
	const parts: string[] = [];
	if (typed?.stdout) parts.push(typed.stdout);
	if (typed?.stderr) parts.push(typed.stderr);
	if (typed?.result) parts.push(typed.result);
	if (typeof content === "string") {
		parts.push(content);
	} else if (Array.isArray(content)) {
		for (const block of content) {
			if (block && typeof block === "object" && "type" in block && block.type === "text" && "text" in block) {
				parts.push(String(block.text));
			}
		}
	}
	return parts.join("\n");
}

function failedBashResult(text: string): boolean {
	if (TRANSPORT_ERROR_RE.test(text)) return true;
	const match = EXIT_CODE_RE.exec(text);
	if (!match) return false;
	return Number(match[1]) !== 0;
}

function assistantHasThinking(message: AgentMessage): boolean {
	if (message.role !== "assistant") return false;
	return message.content.some((block) => block.type === "thinking" && block.thinking.trim().length > 0);
}

function findToolCall(
	messages: readonly AgentMessage[],
	toolCallId: string,
	beforeIndex: number,
): { assistant: AgentMessage; args: unknown } | undefined {
	for (let index = beforeIndex - 1; index >= 0; index--) {
		const message = messages[index];
		if (message?.role !== "assistant") continue;
		for (const block of message.content) {
			if (block.type === "toolCall" && block.id === toolCallId) {
				return { assistant: message, args: block.arguments };
			}
		}
	}
	return undefined;
}

/** classifyToolErrorNudge reports whether one tool result should count toward the nudge streak. */
export function classifyToolErrorNudge(input: {
	toolName: string;
	args: unknown;
	details: unknown;
	isError: boolean;
	content?: unknown;
}): ToolErrorNudgeKind | undefined {
	const details = ipythonDetails(input.details);
	if (details?.status === "aborted") return undefined;
	const ename = details?.error?.ename ?? details?.errorEname;
	if (ename && PYTHON_SYNTAX_ENAMES.has(ename)) {
		return "python-syntax";
	}
	if (input.toolName === "bash") {
		return input.isError ? "bash" : undefined;
	}
	if (input.toolName !== "ipython") return undefined;
	if (!BASH_CALL_RE.test(ipythonCode(input.args))) return undefined;
	if (input.isError || details?.status === "error" || failedBashResult(toolResultText(input.details, input.content))) {
		return "bash";
	}
	return undefined;
}

/** consecutiveToolErrorNudge returns a notice after three failed bash() or SyntaxError cells with no thinking between them. */
export function consecutiveToolErrorNudge(messages: readonly AgentMessage[]): ToolErrorNudge | undefined {
	let count = 0;
	let kind: ToolErrorNudgeKind | undefined;
	for (let index = messages.length - 1; index >= 0; index--) {
		const message = messages[index];
		if (!message) continue;
		if (message.role === "custom" && message.customType === TOOL_ERROR_NUDGE_CUSTOM_TYPE) {
			break;
		}
		if (message.role === "user") {
			break;
		}
		if (message.role === "assistant") {
			if (assistantHasThinking(message)) break;
			continue;
		}
		if (message.role !== "toolResult") {
			continue;
		}
		if (message.toolName !== "ipython" && message.toolName !== "bash") {
			break;
		}
		const call = findToolCall(messages, message.toolCallId, index);
		if (!call || assistantHasThinking(call.assistant)) {
			break;
		}
		const classified = classifyToolErrorNudge({
			toolName: message.toolName,
			args: call.args,
			details: message.details,
			isError: message.isError,
			content: message.content,
		});
		if (!classified) {
			break;
		}
		if (kind && classified !== kind) {
			break;
		}
		kind = classified;
		count += 1;
		if (count >= TOOL_ERROR_NUDGE_THRESHOLD) {
			return {
				kind,
				prompt: kind === "bash" ? BASH_ERROR_NUDGE_PROMPT : PYTHON_SYNTAX_NUDGE_PROMPT,
			};
		}
	}
	return undefined;
}

export function createToolErrorNudgeMessage(
	nudge: ToolErrorNudge,
	timestamp = Date.now(),
): CustomMessage<ToolErrorNudgeDetails> {
	return {
		role: "custom",
		customType: TOOL_ERROR_NUDGE_CUSTOM_TYPE,
		content: nudge.prompt,
		display: true,
		details: { kind: nudge.kind },
		timestamp,
	};
}
