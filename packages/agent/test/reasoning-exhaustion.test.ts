import {
	type AssistantMessage,
	type AssistantMessageEvent,
	EventStream,
	getModel,
	type Message,
	type Model,
	type SimpleStreamOptions,
} from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { transformMessages } from "../../ai/src/providers/transform-messages.js";
import { runAgentLoop } from "../src/agent-loop.js";
import type { AgentLoopConfig, StreamFn } from "../src/types.js";

const model: Model<"openai-completions"> = {
	id: "test-reasoning",
	name: "Test reasoning",
	api: "openai-completions",
	provider: "merge-gateway",
	baseUrl: "https://example.invalid",
	reasoning: true,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 1_000_000,
	maxTokens: 131_000,
};

function response(output: number, exhausted = true): AssistantMessage {
	return {
		role: "assistant",
		api: model.api,
		provider: model.provider,
		model: model.id,
		content: exhausted ? [{ type: "thinking", thinking: "Partial reasoning" }] : [{ type: "text", text: "Answer" }],
		stopReason: exhausted ? "length" : "stop",
		timestamp: 0,
		usage: {
			input: 100,
			output,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 100 + output,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		diagnostics: exhausted
			? [{ type: "provider_warning", timestamp: 0, error: { code: "reasoning_exhausted", message: "No answer" } }]
			: undefined,
	};
}

async function run(responses: AssistantMessage[], overrides: Partial<AgentLoopConfig> = {}) {
	const persisted: Message[] = [];
	const requests: { options: SimpleStreamOptions | undefined; messages: Message[] }[] = [];
	const streamFn: StreamFn = (_model, context, options) => {
		const message = responses[requests.length];
		if (!message) throw new Error("Unexpected provider request");
		requests.push({ options, messages: [...context.messages] });
		const stream = new EventStream<AssistantMessageEvent, AssistantMessage>(
			(event) => event.type === "done",
			() => message,
		);
		stream.push({ type: "done", reason: message.stopReason as "length" | "stop", message });
		return stream;
	};
	const config: AgentLoopConfig = {
		model,
		reasoning: "low",
		convertToLlm: (messages) => messages as Message[],
		...overrides,
	};
	const messages = await runAgentLoop(
		[{ role: "user", content: "Answer the current question", timestamp: 0 }],
		{ systemPrompt: "Answer the user", messages: [], tools: [] },
		config,
		(event) => {
			if (event.type === "message_end") persisted.push(JSON.parse(JSON.stringify(event.message)));
		},
		undefined,
		streamFn,
	);
	return { requests, messages, config, persisted };
}

describe("reasoning exhaustion recovery", () => {
	it("classifies exhaustion before persistence and excludes it on replay", async () => {
		const { persisted } = await run([response(32_000)]);
		expect(persisted.at(-1)).toMatchObject({
			stopReason: "error",
			errorMessage: expect.stringContaining("reasoning"),
		});
		expect(transformMessages(persisted, model).map((message) => message.role)).toEqual(["user"]);
	});

	it("excludes legacy exhausted thinking on same-model and cross-model replay", () => {
		const failed = response(32_000);
		for (const target of [model, getModel("openai", "gpt-4o")]) {
			expect(transformMessages([failed], target)).toEqual([]);
		}
		expect(failed.stopReason).toBe("length");
	});

	it("preserves useful partial answers and tool-associated reasoning", () => {
		for (const content of [
			{ type: "text" as const, text: "Partial answer" },
			{ type: "toolCall" as const, id: "call", name: "read", arguments: {} },
		]) {
			const message = response(32_000);
			message.content.push(content);
			expect(transformMessages([message], model)[0]).toMatchObject({ content: message.content });
		}
	});
	it("does not treat an unused local thinking budget as a provider output limit", async () => {
		const partial = response(32_000);
		const { requests, messages } = await run([partial, response(100, false)], { thinkingBudgets: { low: 1024 } });
		expect(requests.map((request) => request.options?.maxTokens)).toEqual([undefined]);
		expect(messages.at(-1)).toMatchObject({ stopReason: "error" });
	});

	it.each([true, false])("stops with visible reasoning=%s and retains the active request", async (visible) => {
		const partial = response(32_000);
		if (!visible) partial.content = [];
		const { requests, messages, config } = await run([partial, response(100, false)]);
		expect(requests.map((request) => request.options?.maxTokens)).toEqual([undefined]);
		expect(requests[0].options?.reasoning).toBe("low");
		expect(requests[0].messages).toEqual([{ role: "user", content: "Answer the current question", timestamp: 0 }]);
		expect(messages.at(-1)).toMatchObject({ stopReason: "error" });
		expect(config.maxTokens).toBeUndefined();
	});

	it("does not consume queued followups to retry exhausted reasoning", async () => {
		let followedUp = false;
		const { requests } = await run([response(32_000), response(100, false), response(100, false)], {
			getFollowUpMessages: async () => {
				if (followedUp) return [];
				followedUp = true;
				return [{ role: "user", content: "Next question", timestamp: 1 }];
			},
		});
		expect(requests.map((request) => request.options?.maxTokens)).toEqual([undefined]);
		expect(followedUp).toBe(false);
	});

	it("stops after one exhausted response even when the model allows more", async () => {
		const { requests, messages } = await run([response(32_000), response(64_000), response(128_000)]);
		expect(requests.map((request) => request.options?.maxTokens)).toEqual([undefined]);
		expect(messages.at(-1)).toMatchObject({ stopReason: "error" });
	});

	it("does not raise output to the model limit", async () => {
		const { requests, messages } = await run([response(32_000), response(40_000)], {
			model: { ...model, maxTokens: 40_000 },
		});
		expect(requests.map((request) => request.options?.maxTokens)).toEqual([undefined]);
		expect(messages.at(-1)).toMatchObject({ stopReason: "error" });
	});

	it("honors an explicit caller output cap", async () => {
		const { requests, messages } = await run([response(32_000)], { maxTokens: 32_000 });
		expect(requests).toHaveLength(1);
		expect(messages.at(-1)).toMatchObject({ stopReason: "error" });
	});

	it("honors a host stop at the recovery boundary", async () => {
		const { requests } = await run([response(32_000)], { shouldStopAfterTurn: () => true });
		expect(requests).toHaveLength(1);
	});
});
