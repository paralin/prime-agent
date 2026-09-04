import type { ResponseStreamEvent } from "openai/resources/responses/responses.js";
import { describe, expect, it } from "vitest";
import { processResponsesStream } from "../src/providers/openai-responses-shared.js";
import type { AssistantMessage, Model } from "../src/types.js";
import { AssistantMessageEventStream } from "../src/utils/event-stream.js";
import { StreamFailureError } from "../src/utils/stream-failure.js";

const model = {
	id: "gpt-5-codex",
	api: "openai-responses",
	provider: "openai-codex",
} as unknown as Model<"openai-responses">;

function createOutput(): AssistantMessage {
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

async function run(events: ResponseStreamEvent[]): Promise<unknown> {
	const output = createOutput();
	const stream = new AssistantMessageEventStream();
	async function* eventStream() {
		for (const event of events) yield event;
	}

	try {
		await processResponsesStream(eventStream(), output, stream, model);
	} catch (error) {
		return error;
	}
	return undefined;
}

function reasoningEvents(field: "summary" | "content", member: unknown): ResponseStreamEvent[] {
	return [
		{ type: "response.output_item.added", item: { type: "reasoning", id: "rs_1", summary: [] } },
		{
			type: "response.output_item.done",
			item: {
				type: "reasoning",
				id: "rs_1",
				summary: field === "summary" ? [member] : [],
				content: field === "content" ? [member] : [],
			},
		},
	] as unknown as ResponseStreamEvent[];
}

function messageEvents(member: unknown): ResponseStreamEvent[] {
	return [
		{
			type: "response.output_item.added",
			item: { type: "message", id: "msg_1", role: "assistant", status: "in_progress", content: [] },
		},
		{
			type: "response.output_item.done",
			item: { type: "message", id: "msg_1", role: "assistant", status: "completed", content: [member] },
		},
	] as unknown as ResponseStreamEvent[];
}

const cases: Array<{ name: string; events: ResponseStreamEvent[]; detail: string }> = [
	{
		name: "reasoning summary null member",
		events: reasoningEvents("summary", null),
		detail: "output_item.done reasoning summary contained a malformed part",
	},
	{
		name: "reasoning summary non-object member",
		events: reasoningEvents("summary", "garbage"),
		detail: "output_item.done reasoning summary contained a malformed part",
	},
	{
		name: "reasoning summary non-string text",
		events: reasoningEvents("summary", { type: "summary_text", text: null }),
		detail: "output_item.done reasoning summary contained a malformed part",
	},
	{
		name: "reasoning content null member",
		events: reasoningEvents("content", null),
		detail: "output_item.done reasoning content contained a malformed part",
	},
	{
		name: "reasoning content non-object member",
		events: reasoningEvents("content", "garbage"),
		detail: "output_item.done reasoning content contained a malformed part",
	},
	{
		name: "reasoning content non-string text",
		events: reasoningEvents("content", { type: "reasoning_text", text: null }),
		detail: "output_item.done reasoning content contained a malformed part",
	},
	{
		name: "message content null member",
		events: messageEvents(null),
		detail: "output_item.done message content contained a malformed part",
	},
	{
		name: "message content non-object member",
		events: messageEvents("garbage"),
		detail: "output_item.done message content contained a malformed part",
	},
	{
		name: "message output text with non-string text",
		events: messageEvents({ type: "output_text", text: null }),
		detail: "output_item.done message content contained a malformed part",
	},
	{
		name: "message refusal with non-string refusal",
		events: messageEvents({ type: "refusal", refusal: null }),
		detail: "output_item.done message content contained a malformed part",
	},
	{
		name: "message content with unknown type",
		events: messageEvents({ type: "unknown", refusal: "no" }),
		detail: "output_item.done message content contained a malformed part",
	},
];

describe("Responses stream nested output item elements", () => {
	it.each(cases)("rejects $name", async ({ events, detail }) => {
		const error = await run(events);
		expect(error).toBeInstanceOf(StreamFailureError);
		expect((error as StreamFailureError).info).toEqual({
			kind: "malformed_response",
			providerErrorType: "response.output_item.done",
		});
		expect((error as Error).message).toBe(
			`Provider returned a malformed response (response.output_item.done): ${detail}`,
		);
	});
});
