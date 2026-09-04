import { once } from "node:events";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { findEnvKeys, getEnvApiKey } from "../src/env-api-keys.js";
import { streamOpenAICompletions } from "../src/providers/openai-completions.js";
import type { AssistantMessageEvent, Context, Model } from "../src/types.js";

const VENICE_API_KEY = "vk-test-secret-1234";

const originalVeniceApiKey = process.env.VENICE_API_KEY;

afterEach(() => {
	if (originalVeniceApiKey === undefined) {
		delete process.env.VENICE_API_KEY;
	} else {
		process.env.VENICE_API_KEY = originalVeniceApiKey;
	}
});

function veniceModel(baseUrl = "https://api.venice.ai/api/v1"): Model<"openai-completions"> {
	return {
		id: "stealth-ox-alpha",
		name: "Ox Alpha",
		api: "openai-completions",
		provider: "venice",
		baseUrl,
		reasoning: true,
		input: ["text", "image"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 1048576,
		maxTokens: 131072,
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

async function startVeniceMock(
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

describe("Venice provider", () => {
	it("resolves VENICE_API_KEY from the environment", () => {
		process.env.VENICE_API_KEY = VENICE_API_KEY;

		expect(findEnvKeys("venice")).toEqual(["VENICE_API_KEY"]);
		expect(getEnvApiKey("venice")).toBe(VENICE_API_KEY);
	});

	it("posts chat completions to the Venice-compatible endpoint with bearer auth", async () => {
		process.env.VENICE_API_KEY = VENICE_API_KEY;

		function chunk(partial: Record<string, unknown>, finishReason?: string): unknown {
			return {
				id: "chatcmpl-venice-1",
				model: "stealth-ox-alpha",
				choices: [{ index: 0, delta: partial, finish_reason: finishReason ?? null }],
			};
		}

		const { server, url, requests } = await startVeniceMock((request, response) => {
			void request;
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				sse([
					chunk({ role: "assistant" }),
					chunk({ content: "Hello" }),
					chunk({ content: " from Venice" }),
					chunk({}, "stop"),
				]),
			);
		});

		try {
			const events: AssistantMessageEvent[] = [];
			for await (const event of streamOpenAICompletions(veniceModel(url), context)) {
				events.push(event);
			}

			expect(requests).toHaveLength(1);
			const request = requests[0];
			expect(request.path).toBe("/chat/completions");
			expect(request.method).toBe("POST");
			expect(request.authorization).toBe(`Bearer ${VENICE_API_KEY}`);
			expect(request.body.model).toBe("stealth-ox-alpha");

			const done = events.at(-1);
			expect(done?.type).toBe("done");
			if (done?.type !== "done") throw new Error("expected done event");
			expect(done.message.stopReason).toBe("stop");
			expect(
				done.message.content
					.filter((block) => block.type === "text")
					.map((block) => (block as { text: string }).text)
					.join(""),
			).toBe("Hello from Venice");
		} finally {
			await closeServer(server);
		}
	});

	it("maps the length finish reason without treating it as a completed stop", async () => {
		process.env.VENICE_API_KEY = VENICE_API_KEY;

		const { server, url } = await startVeniceMock((request, response) => {
			void request;
			response.writeHead(200, { "content-type": "text/event-stream" });
			response.end(
				sse([
					{
						id: "chatcmpl-venice-2",
						model: "stealth-ox-alpha",
						choices: [{ index: 0, delta: { content: "partial" }, finish_reason: null }],
					},
					{
						id: "chatcmpl-venice-2",
						model: "stealth-ox-alpha",
						choices: [{ index: 0, delta: {}, finish_reason: "length" }],
					},
				]),
			);
		});

		try {
			const events: AssistantMessageEvent[] = [];
			for await (const event of streamOpenAICompletions(veniceModel(url), context)) {
				events.push(event);
			}

			const done = events.at(-1);
			expect(done?.type).toBe("done");
			if (done?.type !== "done") throw new Error("expected done event");
			expect(done.message.stopReason).toBe("length");
		} finally {
			await closeServer(server);
		}
	});

	it("never leaks the bearer key into provider error output", async () => {
		process.env.VENICE_API_KEY = VENICE_API_KEY;

		const { server, url, requests } = await startVeniceMock((request, response) => {
			void request;
			response.writeHead(401, { "content-type": "application/json" });
			response.end(JSON.stringify({ error: { message: "Invalid API key", type: "authentication_error" } }));
		});

		try {
			const events: AssistantMessageEvent[] = [];
			for await (const event of streamOpenAICompletions(veniceModel(url), context)) {
				events.push(event);
			}

			const terminal = events.at(-1);
			expect(terminal?.type).toBe("error");
			if (terminal?.type !== "error") throw new Error("expected provider error");
			expect(requests[0].authorization).toBe(`Bearer ${VENICE_API_KEY}`);
			expect(JSON.stringify(events)).not.toContain(VENICE_API_KEY);
		} finally {
			await closeServer(server);
		}
	});
});
