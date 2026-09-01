import { once } from "node:events";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { findEnvKeys, getEnvApiKey } from "../src/env-api-keys.js";
import { getModel } from "../src/models.js";
import { streamOpenAICompletions } from "../src/providers/openai-completions.js";
import { buildParams, streamOpenAIResponses } from "../src/providers/openai-responses.js";
import type { AssistantMessageEvent, Context, Model } from "../src/types.js";

const MERGE_GATEWAY_API_KEY = "mg-test-secret-1234";
const MODEL_ID = "anthropic/claude-sonnet-4-6";
const MERGE_GATEWAY_BASE_URL = "https://api-gateway.merge.dev/v1";
const originalMergeGatewayApiKey = process.env.MERGE_GATEWAY_API_KEY;

afterEach(() => {
	if (originalMergeGatewayApiKey === undefined) {
		delete process.env.MERGE_GATEWAY_API_KEY;
	} else {
		process.env.MERGE_GATEWAY_API_KEY = originalMergeGatewayApiKey;
	}
});

function mergeGatewayModel(baseUrl = MERGE_GATEWAY_BASE_URL): Model<"openai-completions"> {
	return {
		id: MODEL_ID,
		name: "Claude Sonnet 4.6",
		api: "openai-completions",
		provider: "merge-gateway",
		baseUrl,
		reasoning: true,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 200000,
		maxTokens: 64000,
	};
}

const context: Context = {
	messages: [{ role: "user", content: "hello", timestamp: 0 }],
};

interface CapturedRequest {
	method: string;
	path: string;
	authorization?: string;
	body: any;
}

function sse(chunks: unknown[]): string {
	const lines = chunks.map((chunk) => `data: ${JSON.stringify(chunk)}`);
	lines.push("data: [DONE]");
	return `${lines.join("\n\n")}\n\n`;
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
				body: rawBody ? JSON.parse(rawBody) : undefined,
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

describe("Merge Gateway provider", () => {
	it("resolves MERGE_GATEWAY_API_KEY from the environment", () => {
		process.env.MERGE_GATEWAY_API_KEY = MERGE_GATEWAY_API_KEY;

		expect(findEnvKeys("merge-gateway")).toEqual(["MERGE_GATEWAY_API_KEY"]);
		expect(getEnvApiKey("merge-gateway")).toBe(MERGE_GATEWAY_API_KEY);
	});

	it("posts chat completions to Merge Gateway with bearer auth", async () => {
		process.env.MERGE_GATEWAY_API_KEY = MERGE_GATEWAY_API_KEY;
		const { server, url, requests } = await startMergeGatewayMock((request, response) => {
			void request;
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				sse([
					{
						id: "chatcmpl-merge-gateway-1",
						model: MODEL_ID,
						choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }],
					},
					{
						id: "chatcmpl-merge-gateway-1",
						model: MODEL_ID,
						choices: [{ index: 0, delta: { content: "Hello from Merge Gateway" }, finish_reason: null }],
					},
					{
						id: "chatcmpl-merge-gateway-1",
						model: MODEL_ID,
						choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
					},
				]),
			);
		});

		try {
			const events: AssistantMessageEvent[] = [];
			for await (const event of streamOpenAICompletions(mergeGatewayModel(`${url}/v1`), context)) {
				events.push(event);
			}

			expect(requests).toHaveLength(1);
			const request = requests[0];
			expect(request.path).toBe("/v1/chat/completions");
			expect(request.method).toBe("POST");
			expect(request.authorization).toBe(`Bearer ${MERGE_GATEWAY_API_KEY}`);
			expect(request.body.model).toBe(MODEL_ID);

			const done = events.at(-1);
			expect(done?.type).toBe("done");
			if (done?.type !== "done") throw new Error("expected done event");
			expect(done.message.stopReason).toBe("stop");
			expect(
				done.message.content
					.filter((block) => block.type === "text")
					.map((block) => (block as { text: string }).text)
					.join(""),
			).toBe("Hello from Merge Gateway");
		} finally {
			await closeServer(server);
		}
	});

	it("sends only Merge-supported Responses fields", () => {
		const model = getModel("merge-gateway", "zai/glm-5.3-flash");
		if (!model || model.api !== "openai-responses") throw new Error("expected Merge Responses model");

		const payload = JSON.parse(
			JSON.stringify(
				buildParams(
					model,
					{
						...context,
						systemPrompt: "Use tools when available.",
						messages: [{ role: "user", content: [{ type: "text", text: "hello" }], timestamp: 0 }],
						tools: [{ name: "read", description: "Read a file", parameters: { type: "object" } }],
					},
					{ sessionId: "session-1", reasoningEffort: "high" },
				),
			),
		) as Record<string, unknown>;

		expect(payload).toMatchObject({
			model: "zai/glm-5.3-flash",
			stream: true,
			include_routing_metadata: true,
		});
		expect(payload).not.toHaveProperty("store");
		expect(payload).not.toHaveProperty("prompt_cache_key");
		expect(payload).not.toHaveProperty("prompt_cache_retention");
		// Merge forwards a Responses reasoning field for thinking models; the
		// gateway strips unsupported shape itself, so we send it.
		expect(payload).toHaveProperty("reasoning", { effort: "high", summary: "auto" });
		expect(payload).toHaveProperty("include", ["reasoning.encrypted_content"]);
		expect(payload).toHaveProperty("tools.0.name", "read");
		expect(payload.input).toEqual([
			{ type: "message", role: "system", content: "Use tools when available." },
			{ type: "message", role: "user", content: "hello" },
		]);
	});

	it("reads Merge Gateway cumulative Responses streams", async () => {
		const model = getModel("merge-gateway", "zai/glm-5.3-flash");
		if (!model || model.api !== "openai-responses") throw new Error("expected Merge Responses model");
		const { server, url } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				sse([
					{ object: "response.stream", output: [{ content: [{ type: "text", text: "O" }] }] },
					{ object: "response.stream", output: [{ content: [{ type: "text", text: "OK" }] }] },
					{
						object: "response.done",
						output: [{ finish_reason: "stop", content: [{ type: "text", text: "OK" }] }],
						usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
					},
				]),
			);
		});
		try {
			const result = await streamOpenAIResponses({ ...model, baseUrl: `${url}/v1` }, context, {
				apiKey: MERGE_GATEWAY_API_KEY,
			}).result();
			expect(result.stopReason).toBe("stop");
			expect(result.content).toContainEqual(expect.objectContaining({ type: "text", text: "OK" }));
		} finally {
			await closeServer(server);
		}
	});

	it("reads Merge Gateway tool calls", async () => {
		const model = getModel("merge-gateway", "zai/glm-5.3-flash");
		if (!model || model.api !== "openai-responses") throw new Error("expected Merge Responses model");
		const { server, url } = await startMergeGatewayMock((_request, response) => {
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				sse([
					{
						object: "response.done",
						output: [
							{
								finish_reason: "tool_use",
								content: [{ type: "tool_use", id: "call_1", name: "read", input: { path: "README.md" } }],
							},
						],
						usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
					},
				]),
			);
		});
		try {
			const result = await streamOpenAIResponses({ ...model, baseUrl: `${url}/v1` }, context, {
				apiKey: MERGE_GATEWAY_API_KEY,
			}).result();
			expect(result.stopReason).toBe("toolUse");
			expect(result.content).toContainEqual({
				type: "toolCall",
				id: "call_1",
				name: "read",
				arguments: { path: "README.md" },
			});
		} finally {
			await closeServer(server);
		}
	});

	it("resolves the generated default model", () => {
		const model = getModel("merge-gateway", MODEL_ID);

		expect(model).toBeDefined();
		expect(model).toMatchObject({
			id: MODEL_ID,
			provider: "merge-gateway",
			api: "openai-completions",
			baseUrl: MERGE_GATEWAY_BASE_URL,
		});
	});
});
