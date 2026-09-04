import { beforeEach, describe, expect, it, vi } from "vitest";
import { streamOpenAICompletions } from "../src/providers/openai-completions.js";
import { streamOpenAIResponses } from "../src/providers/openai-responses.js";
import type { Api, Model } from "../src/types.js";
import { getOpenCodeSessionHeaders, OPENCODE_SESSION_HEADER } from "../src/utils/opencode-headers.js";

const mockState = vi.hoisted(() => ({ headers: [] as Record<string, string>[] }));

vi.mock("openai", () => {
	class FakeOpenAI {
		chat = { completions: { create: () => this.request() } };
		responses = { create: () => this.request() };

		constructor(options: { defaultHeaders?: Record<string, string> }) {
			mockState.headers.push(options.defaultHeaders ?? {});
		}

		private request() {
			const data = { async *[Symbol.asyncIterator]() {} };
			return {
				withResponse: async () => ({
					data,
					response: { status: 200, headers: new Headers() },
				}),
			};
		}
	}
	return { default: FakeOpenAI };
});

function model<T extends Api>(api: T, provider: string): Model<T> {
	return {
		id: "test-model",
		name: "Test model",
		api,
		provider,
		baseUrl: provider === "opencode-go" ? "https://opencode.ai/zen/go/v1" : "https://opencode.ai/zen/v1",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 8192,
		maxTokens: 1024,
	} as Model<T>;
}

async function startRequest(api: "openai-completions" | "openai-responses", provider: string, sessionId?: string) {
	const context = { messages: [{ role: "user" as const, content: "hi", timestamp: Date.now() }] };
	const options = { apiKey: "test", sessionId };
	const stream =
		api === "openai-completions"
			? streamOpenAICompletions(model("openai-completions", provider), context, options)
			: streamOpenAIResponses(model("openai-responses", provider), context, options);
	for await (const event of stream) {
		if (event.type === "done" || event.type === "error") break;
	}
}

describe("OpenCode session header", () => {
	beforeEach(() => {
		mockState.headers = [];
	});

	it("returns the conversation header only for OpenCode providers", () => {
		expect(getOpenCodeSessionHeaders("opencode", "conv-1")).toEqual({
			[OPENCODE_SESSION_HEADER]: "conv-1",
		});
		expect(getOpenCodeSessionHeaders("opencode-go", " conv-2 ")).toEqual({
			[OPENCODE_SESSION_HEADER]: "conv-2",
		});
		expect(getOpenCodeSessionHeaders("openrouter", "conv-1")).toEqual({});
		expect(getOpenCodeSessionHeaders("opencode", "  ")).toEqual({});
	});

	it.each(["openai-completions", "openai-responses"] as const)(
		"sends %s OpenCode Zen conversation identity",
		async (api) => {
			await startRequest(api, "opencode", "session-zen");
			expect(mockState.headers).toHaveLength(1);
			expect(mockState.headers[0]?.[OPENCODE_SESSION_HEADER]).toBe("session-zen");
		},
	);

	it.each(["openai-completions", "openai-responses"] as const)(
		"sends %s OpenCode Go conversation identity",
		async (api) => {
			await startRequest(api, "opencode-go", "session-go");
			expect(mockState.headers).toHaveLength(1);
			expect(mockState.headers[0]?.[OPENCODE_SESSION_HEADER]).toBe("session-go");
		},
	);

	it("does not send the header for other completions providers", async () => {
		await startRequest("openai-completions", "openrouter", "session-other");
		expect(mockState.headers).toHaveLength(1);
		expect(mockState.headers[0]?.[OPENCODE_SESSION_HEADER]).toBeUndefined();
	});
});
