import type { AgentMessage, AgentTool } from "@earendil-works/pi-agent-core";
import { fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, describe, expect, it } from "vitest";
import { convertToLlm } from "../src/core/messages.js";
import {
	BASH_ERROR_NUDGE_PROMPT,
	classifyToolErrorNudge,
	consecutiveToolErrorNudge,
	createToolErrorNudgeMessage,
	PYTHON_SYNTAX_NUDGE_PROMPT,
	TOOL_ERROR_NUDGE_CUSTOM_TYPE,
} from "../src/core/tool-error-nudge.js";
import { createHarness, getMessageText, type Harness } from "./suite/harness.js";

const assistantFields = {
	api: "anthropic-messages" as const,
	provider: "anthropic",
	model: "claude-sonnet",
	usage: {
		input: 0,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 0,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	},
	stopReason: "toolUse" as const,
	timestamp: 1,
};

function ipythonCall(id: string, code: string, thinking?: string): AgentMessage {
	return {
		role: "assistant",
		...assistantFields,
		content: [
			...(thinking ? [{ type: "thinking" as const, thinking }] : []),
			{ type: "toolCall", id, name: "ipython", arguments: { code } },
		],
	};
}

function ipythonResult(
	id: string,
	options: {
		isError?: boolean;
		ename?: string;
		status?: "ok" | "error" | "aborted";
		text?: string;
		result?: string;
	},
): AgentMessage {
	const isError = options.isError ?? true;
	return {
		role: "toolResult",
		toolCallId: id,
		toolName: "ipython",
		content: [{ type: "text", text: options.text ?? "" }],
		details: {
			status: options.status ?? (isError ? "error" : "ok"),
			errorEname: options.ename,
			error: options.ename ? { ename: options.ename, evalue: options.ename, traceback: [options.ename] } : undefined,
			result: options.result,
		},
		isError,
		timestamp: 1,
	};
}

function failingIpythonTool(): AgentTool {
	return {
		name: "ipython",
		label: "ipython",
		description: "Python",
		parameters: Type.Object({ code: Type.String() }),
		execute: async (_toolCallId, params) => {
			const code =
				params && typeof params === "object" && "code" in params && typeof params.code === "string"
					? params.code
					: "";
			if (code.includes("syntax")) {
				return {
					content: [{ type: "text", text: "SyntaxError: invalid syntax" }],
					details: {
						status: "error",
						errorEname: "SyntaxError",
						error: { ename: "SyntaxError", evalue: "invalid syntax", traceback: ["SyntaxError"] },
					},
				};
			}
			return {
				content: [{ type: "text", text: "TypeError: command must be a non-empty str" }],
				details: {
					status: "error",
					errorEname: "TypeError",
					error: {
						ename: "TypeError",
						evalue: "command must be a non-empty str",
						traceback: ["TypeError"],
					},
				},
			};
		},
	};
}

describe("classifyToolErrorNudge", () => {
	it("counts Python syntax errors before bash() classification", () => {
		expect(
			classifyToolErrorNudge({
				toolName: "ipython",
				args: { code: "bash(ls)" },
				details: { status: "error", errorEname: "SyntaxError" },
				isError: true,
			}),
		).toBe("python-syntax");
	});

	it("counts failed bash() cells and native bash tool errors", () => {
		expect(
			classifyToolErrorNudge({
				toolName: "ipython",
				args: { code: "await bash('ls')" },
				details: { status: "error", errorEname: "TypeError" },
				isError: true,
			}),
		).toBe("bash");
		expect(
			classifyToolErrorNudge({
				toolName: "ipython",
				args: { code: "await bash('ls missing')" },
				details: {
					status: "ok",
					result: "BashResult(exit_code=2, output='', duration=0.01, transport='local', transport_error=False)",
				},
				isError: false,
			}),
		).toBe("bash");
		expect(
			classifyToolErrorNudge({
				toolName: "bash",
				args: { command: "ls" },
				details: {},
				isError: true,
			}),
		).toBe("bash");
	});

	it("ignores aborted cells, successful bash, and other Python exceptions", () => {
		expect(
			classifyToolErrorNudge({
				toolName: "ipython",
				args: { code: "await bash('ls')" },
				details: { status: "aborted", errorEname: "KeyboardInterrupt" },
				isError: true,
			}),
		).toBeUndefined();
		expect(
			classifyToolErrorNudge({
				toolName: "ipython",
				args: { code: "await bash('ls')" },
				details: {
					status: "ok",
					result: "BashResult(exit_code=0, output='ok', duration=0.01, transport='local', transport_error=False)",
				},
				isError: false,
			}),
		).toBeUndefined();
		expect(
			classifyToolErrorNudge({
				toolName: "ipython",
				args: { code: "raise ValueError('nope')" },
				details: { status: "error", errorEname: "ValueError" },
				isError: true,
			}),
		).toBeUndefined();
	});
});

describe("consecutiveToolErrorNudge", () => {
	it("returns the bash notice after three failed bash() cells with no thinking", () => {
		const messages = [
			{ role: "user" as const, content: "go", timestamp: 1 },
			ipythonCall("1", "await bash('a')"),
			ipythonResult("1", { ename: "TypeError" }),
			ipythonCall("2", "await bash('b')"),
			ipythonResult("2", { ename: "TypeError" }),
			ipythonCall("3", "await bash('c')"),
			ipythonResult("3", { ename: "TypeError" }),
		];
		expect(consecutiveToolErrorNudge(messages)).toEqual({
			kind: "bash",
			prompt: BASH_ERROR_NUDGE_PROMPT,
		});
	});

	it("returns the Python syntax notice after three SyntaxError cells", () => {
		const messages = [
			{ role: "user" as const, content: "go", timestamp: 1 },
			ipythonCall("1", "bash(ls)"),
			ipythonResult("1", { ename: "SyntaxError" }),
			ipythonCall("2", "bash(ls)"),
			ipythonResult("2", { ename: "SyntaxError" }),
			ipythonCall("3", "bash(ls)"),
			ipythonResult("3", { ename: "SyntaxError" }),
		];
		expect(consecutiveToolErrorNudge(messages)).toEqual({
			kind: "python-syntax",
			prompt: PYTHON_SYNTAX_NUDGE_PROMPT,
		});
	});

	it("resets when the model thinks, succeeds, or mixes error kinds", () => {
		expect(
			consecutiveToolErrorNudge([
				ipythonCall("1", "await bash('a')"),
				ipythonResult("1", { ename: "TypeError" }),
				ipythonCall("2", "await bash('b')"),
				ipythonResult("2", { ename: "TypeError" }),
				ipythonCall("3", "await bash('c')", "let me rethink the quoting"),
				ipythonResult("3", { ename: "TypeError" }),
			]),
		).toBeUndefined();
		expect(
			consecutiveToolErrorNudge([
				ipythonCall("1", "await bash('a')"),
				ipythonResult("1", { ename: "TypeError" }),
				ipythonCall("2", "await bash('ok')"),
				ipythonResult("2", {
					isError: false,
					status: "ok",
					result: "BashResult(exit_code=0, output='ok', duration=0.01, transport='local', transport_error=False)",
				}),
				ipythonCall("3", "await bash('c')"),
				ipythonResult("3", { ename: "TypeError" }),
			]),
		).toBeUndefined();
		expect(
			consecutiveToolErrorNudge([
				ipythonCall("1", "await bash('a')"),
				ipythonResult("1", { ename: "TypeError" }),
				ipythonCall("2", "await bash('b')"),
				ipythonResult("2", { ename: "TypeError" }),
				ipythonCall("3", "print("),
				ipythonResult("3", { ename: "SyntaxError" }),
			]),
		).toBeUndefined();
	});
});

describe("convertToLlm tool error nudge", () => {
	it("sends the notice as a system-notice user turn", () => {
		const notice = createToolErrorNudgeMessage({ kind: "bash", prompt: BASH_ERROR_NUDGE_PROMPT }, 10);
		expect(convertToLlm([notice])).toEqual([
			{
				role: "user",
				content: [{ type: "text", text: `<system-notice>\n${BASH_ERROR_NUDGE_PROMPT}\n</system-notice>` }],
				timestamp: 10,
			},
		]);
	});
});

describe("AgentSession tool error nudge", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
	});

	it("steers after three consecutive bash() errors without thinking", async () => {
		const harness = await createHarness({ tools: [failingIpythonTool()] });
		harnesses.push(harness);
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("ipython", { code: "await bash('a')" }), { stopReason: "toolUse" }),
			fauxAssistantMessage(fauxToolCall("ipython", { code: "await bash('b')" }), { stopReason: "toolUse" }),
			fauxAssistantMessage(fauxToolCall("ipython", { code: "await bash('c')" }), { stopReason: "toolUse" }),
			fauxAssistantMessage("I'll step back and fix the syntax."),
		]);

		await harness.session.prompt("run the commands");
		await harness.session.waitForIdle();

		const notice = harness.session.messages.find(
			(message) => message.role === "custom" && message.customType === TOOL_ERROR_NUDGE_CUSTOM_TYPE,
		);
		expect(notice).toBeDefined();
		expect(getMessageText(notice)).toBe(BASH_ERROR_NUDGE_PROMPT);
		expect(harness.session.messages.some((message) => getMessageText(message).includes("I'll step back"))).toBe(true);
	});

	it("steers after three consecutive Python syntax errors without thinking", async () => {
		const harness = await createHarness({ tools: [failingIpythonTool()] });
		harnesses.push(harness);
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("ipython", { code: "syntax 1" }), { stopReason: "toolUse" }),
			fauxAssistantMessage(fauxToolCall("ipython", { code: "syntax 2" }), { stopReason: "toolUse" }),
			fauxAssistantMessage(fauxToolCall("ipython", { code: "syntax 3" }), { stopReason: "toolUse" }),
			fauxAssistantMessage("I'll reread the Python."),
		]);

		await harness.session.prompt("run the cells");
		await harness.session.waitForIdle();

		const notice = harness.session.messages.find(
			(message) => message.role === "custom" && message.customType === TOOL_ERROR_NUDGE_CUSTOM_TYPE,
		);
		expect(getMessageText(notice)).toBe(PYTHON_SYNTAX_NUDGE_PROMPT);
	});
});
