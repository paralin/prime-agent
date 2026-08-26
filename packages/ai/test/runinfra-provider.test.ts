import { once } from "node:events";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { findEnvKeys, getEnvApiKey } from "../src/env-api-keys.js";
import { getModel, getModels } from "../src/models.js";
import { streamOpenAICompletions } from "../src/providers/openai-completions.js";
import type { AssistantMessageEvent, Context, Model } from "../src/types.js";

const RUNINFRA_GATEWAY_KEY = "rp_test_secret";
const originalRunInfraKey = process.env.RUNINFRA_GATEWAY_KEY;

afterEach(() => {
	if (originalRunInfraKey === undefined) delete process.env.RUNINFRA_GATEWAY_KEY;
	else process.env.RUNINFRA_GATEWAY_KEY = originalRunInfraKey;
});

describe("RunInfra provider", () => {
	it("registers the documented model catalog and environment key", () => {
		process.env.RUNINFRA_GATEWAY_KEY = RUNINFRA_GATEWAY_KEY;

		expect(findEnvKeys("runinfra")).toEqual(["RUNINFRA_GATEWAY_KEY"]);
		expect(getEnvApiKey("runinfra")).toBe(RUNINFRA_GATEWAY_KEY);
		expect(getModels("runinfra").map((model) => model.id)).toEqual([
			"deepseek-v4-flash",
			"deepseek-v4-pro",
			"nemotron-3-5-lightning-30b",
			"ornith-1-5-35b",
			"qwen3-8-2-4t-a95b",
			"qwen3-8-27b",
		]);
		expect(getModel("runinfra", "deepseek-v4-flash")).toMatchObject({
			api: "openai-completions",
			baseUrl: "https://api.runinfra.ai/v1",
			contextWindow: 1_048_576,
			maxTokens: 1_048_576,
		});
	});

	it("posts streaming chat completions with bearer auth", async () => {
		process.env.RUNINFRA_GATEWAY_KEY = RUNINFRA_GATEWAY_KEY;
		let requestPath: string | undefined;
		let authorization: string | undefined;
		let requestBody: Record<string, unknown> | undefined;
		const server = http.createServer((request, response) => {
			let body = "";
			request.on("data", (chunk) => {
				body += chunk;
			});
			request.on("end", () => {
				requestPath = request.url;
				authorization = request.headers.authorization;
				requestBody = JSON.parse(body);
				response.writeHead(200, { "content-type": "text/event-stream" });
				response.end(
					'data: {"id":"chatcmpl-runinfra","choices":[{"index":0,"delta":{"content":"ready"},"finish_reason":null}]}\n\n' +
						'data: {"id":"chatcmpl-runinfra","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n' +
						"data: [DONE]\n\n",
				);
			});
		});
		server.listen(0, "127.0.0.1");
		await once(server, "listening");
		const { port } = server.address() as AddressInfo;
		const model: Model<"openai-completions"> = {
			...getModel("runinfra", "deepseek-v4-flash"),
			baseUrl: `http://127.0.0.1:${port}/v1`,
		};
		const context: Context = { messages: [{ role: "user", content: "hello", timestamp: 0 }] };

		try {
			const events: AssistantMessageEvent[] = [];
			for await (const event of streamOpenAICompletions(model, context)) events.push(event);

			expect(requestPath).toBe("/v1/chat/completions");
			expect(authorization).toBe(`Bearer ${RUNINFRA_GATEWAY_KEY}`);
			expect(requestBody?.model).toBe("deepseek-v4-flash");
			expect(events.at(-1)?.type).toBe("done");
		} finally {
			server.closeAllConnections();
			server.close();
			if (server.listening) await once(server, "close");
		}
	});
});
