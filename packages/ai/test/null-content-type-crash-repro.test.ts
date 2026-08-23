import type { ResponseStreamEvent } from "openai/resources/responses/responses.js";
import { describe, expect, it } from "vitest";
import { processResponsesStream } from "../src/providers/openai-responses-shared.js";
import type { AssistantMessage, Model } from "../src/types.js";
import { AssistantMessageEventStream } from "../src/utils/event-stream.js";
import { StreamFailureError } from "../src/utils/stream-failure.js";

// Minimal deterministic reproduction of the live-session crash
// "Cannot read properties of null (reading 'type')": a responses-API
// stream event whose payload object is null crosses mapCodexEvents
// unguarded and reaches the discriminant read in processResponsesStream.

function createOutput(model: Model<"openai-responses">): AssistantMessage {
	return {
		role: "assistant",
		content: [],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop",
		timestamp: Date.now(),
	};
}

const model = {
	id: "gpt-5-codex",
	api: "openai-responses",
	provider: "openai-codex",
} as unknown as Model<"openai-responses">;

async function run(events: ResponseStreamEvent[]): Promise<unknown> {
	const output = createOutput(model);
	const stream = new AssistantMessageEventStream();
	let observed: unknown;
	const drain = (async () => {
		try {
			for await (const _ of stream) {
				// consume
			}
		} catch (error) {
			observed = error;
		}
	})();
	async function* events_() {
		for (const event of events) yield event;
	}
	try {
		await processResponsesStream(events_(), output, stream, model);
	} catch (error) {
		observed = error;
	}
	stream.end();
	await drain;
	return observed;
}

describe("null payload content crash", () => {
	it("throws StreamFailureError(malformed_response) on response.content_part.added with part: null", async () => {
		const error = await run([
			{
				type: "response.output_item.added",
				item: { type: "message", id: "msg_1", role: "assistant", content: [] },
			},
			{ type: "response.content_part.added", part: null },
		] as unknown as ResponseStreamEvent[]);
		expect(error).toBeInstanceOf(StreamFailureError);
		expect((error as StreamFailureError).info.kind).toBe("malformed_response");
		expect((error as Error).message).toBe(
			"Provider returned a malformed response (response.content_part.added): content_part.added carried no content part",
		);
	});

	it("throws StreamFailureError(malformed_response) on response.output_item.done with item: null", async () => {
		const error = await run([{ type: "response.output_item.done", item: null }] as unknown as ResponseStreamEvent[]);
		expect(error).toBeInstanceOf(StreamFailureError);
		expect((error as StreamFailureError).info.kind).toBe("malformed_response");
		expect((error as Error).message).toBe(
			"Provider returned a malformed response (response.output_item.done): output_item.done carried no output item",
		);
	});

	it("throws StreamFailureError(malformed_response) on response.output_item.added with item: null", async () => {
		const error = await run([{ type: "response.output_item.added", item: null }] as unknown as ResponseStreamEvent[]);
		expect(error).toBeInstanceOf(StreamFailureError);
		expect((error as StreamFailureError).info.kind).toBe("malformed_response");
		expect((error as Error).message).toBe(
			"Provider returned a malformed response (response.output_item.added): output_item.added carried no output item",
		);
	});
});
