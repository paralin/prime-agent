import { once } from "node:events";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { findEnvKeys, getEnvApiKey } from "../src/env-api-keys.js";
import { getModel, getSupportedThinkingLevels } from "../src/models.js";
import { streamOpenAICompletions, streamSimpleOpenAICompletions } from "../src/providers/openai-completions.js";
import type { AssistantMessageEvent, Context, Model } from "../src/types.js";

const MERGE_GATEWAY_API_KEY = "mg-test-secret-1234";
const MODEL_ID = "zai/glm-5.3-flash";
const MERGE_GATEWAY_BASE_URL = "https://api-gateway.merge.dev/v1/ai-sdk";
const SIGNED_INSPECTION = JSON.stringify({
	type: "openai-completions.chat_thinking_signature.v1",
	reasoningField: "thinking",
	signatureField: "thinking_signature",
	signature: "signed-inspection",
});
const originalMergeGatewayApiKey = process.env.MERGE_GATEWAY_API_KEY;

afterEach(() => {
	if (originalMergeGatewayApiKey === undefined) {
		delete process.env.MERGE_GATEWAY_API_KEY;
	} else {
		process.env.MERGE_GATEWAY_API_KEY = originalMergeGatewayApiKey;
	}
});

interface CapturedRequest {
	method: string;
	path: string;
	authorization?: string;
	sessionId?: string;
	sessionAffinity?: string;
	legacySessionId?: string;
	clientRequestId?: string;
	body: Record<string, unknown>;
}

function chatSse(frames: Array<Record<string, unknown>>): string {
	return `${frames.map((frame) => `data: ${JSON.stringify(frame)}`).join("\n\n")}\n\ndata: [DONE]\n\n`;
}

async function startMergeGatewayMock(
	handler: (request: http.IncomingMessage, response: http.ServerResponse) => void,
): Promise<{ server: http.Server; url: string; requests: CapturedRequest[] }> {
	const requests: CapturedRequest[] = [];
	const server = http.createServer((request, response) => {
		let rawBody = "";
		request.on("data", (data) => {
			rawBody += data;
		});
		request.on("end", () => {
			requests.push({
				method: request.method ?? "",
				path: request.url ?? "",
				authorization: request.headers.authorization,
				sessionId: request.headers["x-session-id"] as string | undefined,
				sessionAffinity: request.headers["x-session-affinity"] as string | undefined,
				legacySessionId: request.headers.session_id as string | undefined,
				clientRequestId: request.headers["x-client-request-id"] as string | undefined,
				body: JSON.parse(rawBody) as Record<string, unknown>,
			});
			handler(request, response);
		});
	});
	server.listen(0, "127.0.0.1");
	await once(server, "listening");
	const { port } = server.address() as AddressInfo;
	return { server, url: `http://127.0.0.1:${port}`, requests };
}

async function closeServer(server: http.Server): Promise<void> {
	server.closeAllConnections();
	server.close();
	if (server.listening) await once(server, "close");
}

function mergeModel(baseUrl = MERGE_GATEWAY_BASE_URL): Model<"openai-completions"> {
	const model = getModel("merge-gateway", MODEL_ID);
	if (!model || model.api !== "openai-completions") throw new Error("expected Merge OpenAI Chat model");
	return { ...model, baseUrl };
}

const context: Context = {
	systemPrompt: "Use tools when available.",
	messages: [
		{
			role: "user",
			content: [
				{ type: "text", text: "Inspect this image." },
				{ type: "image", mimeType: "image/png", data: "iVBORw0KGgo=" },
			],
			timestamp: 0,
		},
		{
			role: "assistant",
			content: [
				{ type: "thinking", thinking: "Inspecting the image.", thinkingSignature: SIGNED_INSPECTION },
				{ type: "text", text: "Let me look." },
				{ type: "toolCall", id: "call_1", name: "read", arguments: { path: "image.png" } },
			],
			api: "openai-completions",
			provider: "merge-gateway",
			model: MODEL_ID,
			usage: {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "toolUse",
			timestamp: 1,
		},
		{
			role: "toolResult",
			toolCallId: "call_1",
			toolName: "read",
			content: [{ type: "text", text: "image contents" }],
			isError: false,
			timestamp: 2,
		},
		{ role: "user", content: "Continue", timestamp: 3 },
	],
	tools: [{ name: "read", description: "Read a file", parameters: { type: "object" } }],
};

describe("Merge Gateway provider", () => {
	it("resolves MERGE_GATEWAY_API_KEY from the environment", () => {
		process.env.MERGE_GATEWAY_API_KEY = MERGE_GATEWAY_API_KEY;

		expect(findEnvKeys("merge-gateway")).toEqual(["MERGE_GATEWAY_API_KEY"]);
		expect(getEnvApiKey("merge-gateway")).toBe(MERGE_GATEWAY_API_KEY);
	});

	it("routes generated models through the OpenCode Chat endpoint and compatibility defaults", () => {
		const model = mergeModel();
		expect(model).toMatchObject({
			id: MODEL_ID,
			provider: "merge-gateway",
			api: "openai-completions",
			baseUrl: MERGE_GATEWAY_BASE_URL,
			compat: {
				reasoningField: "thinking",
				requireFinishReason: true,
				supportsStore: true,
				supportsDeveloperRole: false,
				supportsReasoningEffort: true,
				maxTokensField: "max_tokens",
				thinkingFormat: "merge",
				sendSessionAffinityHeaders: ["x-session-affinity", "X-Session-Id"],
			},
			cost: { input: 0.015, output: 0.05, cacheRead: 0.003, cacheWrite: 0 },
		});
		expect(getSupportedThinkingLevels(model)).toEqual(["low", "high", "max"]);
	});

	it.each([
		["low", 1_024],
		["high", 4_096],
		["max", 16_384],
	] as const)("sends the selected %s reasoning effort and token budget", async (effort, budgetTokens) => {
		const { server, url, requests } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(chatSse([{ id: "chat_effort", choices: [{ delta: { content: "ok" }, finish_reason: "stop" }] }]));
		});

		try {
			const result = await streamSimpleOpenAICompletions(
				mergeModel(`${url}/v1/ai-sdk`),
				{ messages: [{ role: "user", content: "hello", timestamp: 0 }] },
				{
					apiKey: MERGE_GATEWAY_API_KEY,
					reasoning: effort,
					thinkingBudgets: { [effort]: budgetTokens },
				},
			).result();

			expect(result.stopReason).toBe("stop");
			expect(requests[0]?.body.reasoning_effort).toBe(effort);
			expect(requests[0]?.body.thinking).toEqual({ type: "enabled", budget_tokens: budgetTokens });
		} finally {
			await closeServer(server);
		}
	});

	it("sends the OpenCode request shape and parses Chat thinking, tools, and usage", async () => {
		const { server, url, requests } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				chatSse([
					{ id: "chat_1", model: MODEL_ID, choices: [{ delta: { thinking: "Plan" }, finish_reason: null }] },
					{
						id: "chat_1",
						model: MODEL_ID,
						choices: [{ delta: { thinking_signature: "signed-plan" }, finish_reason: null }],
					},
					{ id: "chat_1", model: MODEL_ID, choices: [{ delta: { content: "Done." }, finish_reason: null }] },
					{
						id: "chat_1",
						model: MODEL_ID,
						choices: [
							{
								delta: {
									tool_calls: [
										{
											index: 0,
											id: "call_2",
											type: "function",
											function: { name: "read", arguments: '{"path":"next.txt"}' },
										},
									],
								},
								finish_reason: "tool_calls",
							},
						],
					},
					{
						id: "chat_1",
						model: MODEL_ID,
						choices: [],
						usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15, cached_tokens: 2 },
					},
				]),
			);
		});

		try {
			const events: AssistantMessageEvent[] = [];
			const stream = streamOpenAICompletions(mergeModel(`${url}/v1/ai-sdk`), context, {
				apiKey: MERGE_GATEWAY_API_KEY,
				maxTokens: 100,
				reasoningEffort: "high",
				reasoningBudgetTokens: 4_096,
				sessionId: "merge-session-1",
			});
			for await (const event of stream) events.push(event);
			const result = await stream.result();

			expect(requests).toHaveLength(1);
			expect(requests[0]).toMatchObject({
				method: "POST",
				path: "/v1/ai-sdk/chat/completions",
				authorization: `Bearer ${MERGE_GATEWAY_API_KEY}`,
				sessionId: "merge-session-1",
				sessionAffinity: "merge-session-1",
			});
			expect(requests[0]?.legacySessionId).toBeUndefined();
			expect(requests[0]?.clientRequestId).toBeUndefined();
			expect(requests[0]?.body).toEqual({
				model: MODEL_ID,
				messages: [
					{ role: "system", content: "Use tools when available." },
					{
						role: "user",
						content: [
							{ type: "text", text: "Inspect this image." },
							{ type: "image_url", image_url: { url: "data:image/png;base64,iVBORw0KGgo=" } },
						],
					},
					{
						role: "assistant",
						content: "Let me look.",
						thinking: "Inspecting the image.",
						thinking_signature: "signed-inspection",
						tool_calls: [
							{
								id: "call_1",
								type: "function",
								function: { name: "read", arguments: '{"path":"image.png"}' },
							},
						],
					},
					{ role: "tool", content: "image contents", tool_call_id: "call_1" },
					{ role: "user", content: "Continue" },
				],
				stream: true,
				stream_options: { include_usage: true },
				store: false,
				max_tokens: 100,
				tools: [
					{
						type: "function",
						function: {
							name: "read",
							description: "Read a file",
							parameters: { type: "object" },
							strict: false,
						},
					},
				],
				reasoning_effort: "high",
				thinking: { type: "enabled", budget_tokens: 4_096 },
			});
			expect(result.stopReason).toBe("toolUse");
			expect(result.content).toEqual([
				expect.objectContaining({
					type: "thinking",
					thinking: "Plan",
					thinkingSignature: expect.stringContaining("signed-plan"),
				}),
				expect.objectContaining({ type: "text", text: "Done." }),
				expect.objectContaining({ type: "toolCall", id: "call_2", name: "read", arguments: { path: "next.txt" } }),
			]);
			expect(events.some((event) => event.type === "thinking_delta" && event.delta === "Plan")).toBe(true);
			expect(result.usage).toMatchObject({ input: 8, output: 5, cacheRead: 2, totalTokens: 15 });
		} finally {
			await closeServer(server);
		}
	});

	it("rejects a Chat stream that closes without finish_reason", async () => {
		const { server, url } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				chatSse([{ id: "chat_partial", choices: [{ delta: { content: "partial" }, finish_reason: null }] }]),
			);
		});

		try {
			const result = await streamOpenAICompletions(
				mergeModel(`${url}/v1/ai-sdk`),
				{ messages: [{ role: "user", content: "hello", timestamp: 0 }] },
				{ apiKey: MERGE_GATEWAY_API_KEY },
			).result();

			expect(result.stopReason).toBe("error");
			expect(result.errorMessage).toContain("stream ended without finish_reason");
		} finally {
			await closeServer(server);
		}
	});

	it("preserves terminal warnings as assistant diagnostics", async () => {
		const warning = {
			code: "reasoning_exhausted",
			message: "the model spent its whole thinking budget and returned no answer",
			detail: { model: MODEL_ID, vendor: "particle" },
		};
		const { server, url } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				chatSse([
					{
						id: "chat_warning",
						choices: [{ delta: { thinking: "Still reasoning." }, finish_reason: "length" }],
					},
					{ id: "chat_warning", choices: [], warnings: [warning] },
				]),
			);
		});

		try {
			const result = await streamOpenAICompletions(
				mergeModel(`${url}/v1/ai-sdk`),
				{ messages: [{ role: "user", content: "hello", timestamp: 0 }] },
				{ apiKey: MERGE_GATEWAY_API_KEY },
			).result();

			expect(result.stopReason).toBe("length");
			expect(result.diagnostics).toEqual([
				{
					type: "provider_warning",
					timestamp: expect.any(Number),
					error: { code: warning.code, message: warning.message },
					details: { detail: warning.detail },
				},
			]);
		} finally {
			await closeServer(server);
		}
	});

	it("rejects output received after finish_reason", async () => {
		const { server, url } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				chatSse([
					{ id: "chat_done", choices: [{ delta: { content: "Done." }, finish_reason: "stop" }] },
					{ id: "chat_done", choices: [{ delta: { content: " stale" }, finish_reason: null }] },
				]),
			);
		});

		try {
			const result = await streamOpenAICompletions(
				mergeModel(`${url}/v1/ai-sdk`),
				{ messages: [{ role: "user", content: "hello", timestamp: 0 }] },
				{ apiKey: MERGE_GATEWAY_API_KEY },
			).result();

			expect(result.stopReason).toBe("error");
			expect(result.errorMessage).toContain("content after the finish reason");
		} finally {
			await closeServer(server);
		}
	});

	it("rejects terminal tool calls without an id or name", async () => {
		const { server, url } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				chatSse([
					{
						id: "chat_tool",
						choices: [
							{
								delta: { tool_calls: [{ index: 0, type: "function", function: { arguments: "{}" } }] },
								finish_reason: "tool_calls",
							},
						],
					},
				]),
			);
		});

		try {
			const result = await streamOpenAICompletions(
				mergeModel(`${url}/v1/ai-sdk`),
				{ messages: [{ role: "user", content: "hello", timestamp: 0 }] },
				{ apiKey: MERGE_GATEWAY_API_KEY },
			).result();

			expect(result.stopReason).toBe("error");
			expect(result.errorMessage).toContain("tool call delta is missing id or name");
		} finally {
			await closeServer(server);
		}
	});
});
