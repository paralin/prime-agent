import { once } from "node:events";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { streamOpenAICompletions } from "../src/providers/openai-completions.js";
import type { AssistantMessageEvent, Context, Model, Usage } from "../src/types.js";

const emptyUsage: Usage = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

function buildModel(baseUrl: string): Model<"openai-completions"> {
	return {
		id: "repro-model",
		name: "Repro Model",
		api: "openai-completions",
		provider: "repro-provider",
		baseUrl,
		reasoning: true,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 128000,
		maxTokens: 4096,
	};
}

const context: Context = {
	messages: [{ role: "user", content: "hello", timestamp: 1 }],
};

async function collectEvents(stream: AsyncIterable<AssistantMessageEvent>): Promise<AssistantMessageEvent[]> {
	const events: AssistantMessageEvent[] = [];
	for await (const event of stream) events.push(event);
	return events;
}

async function listen(server: http.Server): Promise<number> {
	server.listen(0, "127.0.0.1");
	await once(server, "listening");
	return (server.address() as AddressInfo).port;
}

// Live failure signature: a partial thinking delta and zero usage arrive, then
// the provider sends [DONE] or closes the socket without a terminal chunk, so
// no choice ever carries a truthy finish_reason.
function incompleteStreamBody(endWithDone: boolean): string[] {
	const chunk = {
		id: "chatcmpl-incomplete",
		object: "chat.completion.chunk",
		created: 0,
		model: "repro-model",
		choices: [
			{
				index: 0,
				delta: { role: "assistant", reasoning_content: "partial reasoning" },
				finish_reason: null,
			},
		],
	};
	const frames = [`data: ${JSON.stringify(chunk)}\n\n`];
	if (endWithDone) frames.push("data: [DONE]\n\n");
	return frames;
}

describe("openai-completions missing finish reason", () => {
	afterEach(() => {
		delete process.env.OPENAI_API_KEY;
	});

	it.each([true, false])(
		"reports stopReason unknown when the stream ends without finish_reason (done=%s)",
		async (endWithDone) => {
			const server = http.createServer((_req, res) => {
				res.writeHead(200, {
					"content-type": "text/event-stream",
					"cache-control": "no-cache",
					connection: "keep-alive",
				});
				for (const frame of incompleteStreamBody(endWithDone)) res.write(frame);
				res.end();
			});
			const port = await listen(server);

			try {
				const events = await collectEvents(
					streamOpenAICompletions(buildModel(`http://127.0.0.1:${port}`), context, { apiKey: "test-key" }),
				);

				const thinking = events.find((event) => event.type === "thinking_delta");
				expect(thinking).toBeDefined();

				const terminal = events.at(-1);
				expect(terminal?.type).toBe("done");
				if (terminal?.type !== "done") throw new Error("expected done event");
				expect(terminal.reason).toBe("unknown");
				expect(terminal.message.stopReason).toBe("unknown");
				expect(terminal.message.usage).toEqual(emptyUsage);
				expect(terminal.message.content).toContainEqual({
					type: "thinking",
					thinking: "partial reasoning",
					thinkingSignature: "reasoning_content",
				});
			} finally {
				server.closeAllConnections();
				server.close();
				await once(server, "close");
			}
		},
	);

	it("still reports stopReason stop on a real terminal chunk", async () => {
		const server = http.createServer((_req, res) => {
			res.writeHead(200, {
				"content-type": "text/event-stream",
				"cache-control": "no-cache",
				connection: "keep-alive",
			});
			res.write(
				`data: ${JSON.stringify({
					id: "chatcmpl-complete",
					object: "chat.completion.chunk",
					created: 0,
					model: "repro-model",
					choices: [{ index: 0, delta: { content: "ok" }, finish_reason: null }],
				})}\n\n`,
			);
			res.write(
				`data: ${JSON.stringify({
					id: "chatcmpl-complete",
					object: "chat.completion.chunk",
					created: 0,
					model: "repro-model",
					choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
				})}\n\n`,
			);
			res.end("data: [DONE]\n\n");
		});
		const port = await listen(server);

		try {
			const events = await collectEvents(
				streamOpenAICompletions(buildModel(`http://127.0.0.1:${port}`), context, { apiKey: "test-key" }),
			);

			const terminal = events.at(-1);
			expect(terminal?.type).toBe("done");
			if (terminal?.type !== "done") throw new Error("expected done event");
			expect(terminal.reason).toBe("stop");
		} finally {
			server.closeAllConnections();
			server.close();
			await once(server, "close");
		}
	});
});
