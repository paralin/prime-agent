import type { ResponseStreamEvent } from "openai/resources/responses/responses.js";
import { describe, expect, it } from "vitest";
import { processResponsesStream } from "../src/providers/openai-responses-shared.js";
import type { AssistantMessage, Model } from "../src/types.js";
import { AssistantMessageEventStream } from "../src/utils/event-stream.js";
import { StreamFailureError } from "../src/utils/stream-failure.js";

// Regression coverage for the Responses stream null-payload guards. Each
// test drives the real exported processResponsesStream with crafted wire
// events carrying a null or malformed payload at one of the eight guarded
// boundaries and asserts the bounded StreamFailureError that crosses the
// stream instead of the raw TypeError the unguarded parser raised.

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

function expectMalformed(error: unknown, providerErrorType: string, detail: string): void {
	expect(error).toBeInstanceOf(StreamFailureError);
	expect((error as StreamFailureError).info.kind).toBe("malformed_response");
	expect((error as StreamFailureError).info.providerErrorType).toBe(providerErrorType);
	expect((error as Error).message).toBe(`Provider returned a malformed response (${providerErrorType}): ${detail}`);
}

describe("Responses stream null payload boundaries", () => {
	it("boundary 1: response.created with response: null crashes reading id", async () => {
		const error = await run([{ type: "response.created", response: null }] as unknown as ResponseStreamEvent[]);
		expectMalformed(error, "response.created", "response.created carried no response");
	});

	it("boundary 2: output_item.added with item: null crashes reading type", async () => {
		const error = await run([{ type: "response.output_item.added", item: null }] as unknown as ResponseStreamEvent[]);
		expectMalformed(error, "response.output_item.added", "output_item.added carried no item");
	});

	it("boundary 3: reasoning_summary_part.added with part: null crashes at done-time summary mapping", async () => {
		// The same item object travels added -> done on a real stream; the
		// summary_part.added handler pushes event.part into its summary, and
		// the unguarded crash surfaces when output_item.done maps that
		// summary back into text.
		const reasoningItem = { type: "reasoning", id: "rs_1", summary: [] as unknown[] };
		const error = await run([
			{ type: "response.output_item.added", item: reasoningItem },
			{ type: "response.reasoning_summary_part.added", part: null },
			{ type: "response.output_item.done", item: reasoningItem },
		] as unknown as ResponseStreamEvent[]);
		expectMalformed(
			error,
			"response.reasoning_summary_part.added",
			"reasoning_summary_part.added carried no summary part",
		);
	});

	it("boundary 4: content_part.added with part: null crashes reading type", async () => {
		const error = await run([
			{
				type: "response.output_item.added",
				item: { type: "message", id: "msg_1", role: "assistant", content: [] },
			},
			{ type: "response.content_part.added", part: null },
		] as unknown as ResponseStreamEvent[]);
		expectMalformed(error, "response.content_part.added", "content_part.added carried no content part");
	});

	it("boundary 5: output_item.done with item: null crashes reading type", async () => {
		const error = await run([{ type: "response.output_item.done", item: null }] as unknown as ResponseStreamEvent[]);
		expectMalformed(error, "response.output_item.done", "output_item.done carried no item");
	});

	it("boundary 6: output_item.done for a message with content: null crashes reading map", async () => {
		const error = await run([
			{
				type: "response.output_item.added",
				item: { type: "message", id: "msg_1", role: "assistant", content: [] },
			},
			{
				type: "response.output_item.done",
				item: { type: "message", id: "msg_1", role: "assistant", content: null },
			},
		] as unknown as ResponseStreamEvent[]);
		expectMalformed(error, "response.output_item.done", "output_item.done message carried no content");
	});

	it("boundary 7: output_item.done for reasoning with non-array summary crashes at summary mapping", async () => {
		const error = await run([
			{
				type: "response.output_item.added",
				item: { type: "reasoning", id: "rs_1", summary: [] },
			},
			{
				type: "response.output_item.done",
				item: { type: "reasoning", id: "rs_1", summary: "garbage" },
			},
		] as unknown as ResponseStreamEvent[]);
		expectMalformed(error, "response.output_item.done", "output_item.done reasoning carried no summary");
	});

	it("boundary 8: function_call_arguments.done with arguments: null crashes at the startsWith check", async () => {
		// Pre-fix, parseStreamingJson(null) survived and the stream crashed
		// afterwards, when event.arguments.startsWith(previousPartialJson)
		// read startsWith off a non-string; the guard classifies first.
		const error = await run([
			{
				type: "response.output_item.added",
				item: { type: "function_call", id: "fc_1", call_id: "call_1", name: "get_weather" },
			},
			{ type: "response.function_call_arguments.delta", delta: '{"city":' },
			{ type: "response.function_call_arguments.done", arguments: null },
		] as unknown as ResponseStreamEvent[]);
		expectMalformed(
			error,
			"response.function_call_arguments.done",
			"function_call_arguments.done carried no arguments",
		);
	});
});
