import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import { createHarness, getUserTexts, type Harness } from "./harness.js";
import { gatedHook, withStreaming } from "./scheduling.js";

describe("AgentSession action contracts", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
	});

	it("parses session commands only through prompt provenance", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("literal handled")]);

		await harness.session.prompt("/compact");
		expect(harness.getPendingResponseCount()).toBe(1);
		expect(
			harness.session.messages.some(
				(message) => message.role === "custom" && message.customType === "session_slash_command",
			),
		).toBe(true);

		await harness.session.steer("/compact", undefined, { resumeIfIdle: true });
		await harness.session.waitForIdle();

		expect(getUserTexts(harness)).toEqual(["/compact"]);
		expect(harness.getPendingResponseCount()).toBe(0);
	});

	it("runs input handlers before deciding whether busy submissions enter a queue", async () => {
		const harness = await createHarness({
			extensionFactories: [
				(pi) => {
					pi.on("input", async (event) =>
						event.text === "handled"
							? { action: "handled" }
							: { action: "transform", text: `transformed:${event.text}` },
					);
				},
			],
		});
		harnesses.push(harness);
		withStreaming(harness, true);

		await harness.session.prompt("queued", { streamingBehavior: "followUp" });
		await harness.session.prompt("handled", { streamingBehavior: "followUp" });

		expect(harness.session.getFollowUpMessages()).toEqual(["transformed:queued"]);
		expect(harness.session.queuedActionCount).toBe(1);
		withStreaming(harness, false);
		expect(harness.session.clearQueue()).toEqual({ steering: [], followUp: ["transformed:queued"] });
	});

	it("admits external session messages as literal steering while busy", async () => {
		let inputHandlerRuns = 0;
		const harness = await createHarness({
			extensionFactories: [
				(pi) => {
					pi.on("input", async () => {
						inputHandlerRuns++;
						return { action: "transform", text: "rewritten" };
					});
				},
			],
		});
		harnesses.push(harness);
		withStreaming(harness, true);

		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", {
				message: "/compact external text\n ",
				key: "matrix:$busy",
			}),
		).resolves.toEqual({ accepted: true, deliveryStatus: "queued", key: "matrix:$busy" });
		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", {
				message: "duplicate body is ignored",
				key: "matrix:$busy",
			}),
		).resolves.toEqual({ accepted: true, deliveryStatus: "coalesced", key: "matrix:$busy" });
		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", {
				message: "distinct event",
				key: "matrix:$other",
			}),
		).resolves.toEqual({ accepted: true, deliveryStatus: "queued", key: "matrix:$other" });

		expect(inputHandlerRuns).toBe(0);
		expect(harness.session.getSteeringMessages()).toEqual(["/compact external text\n ", "distinct event"]);
		withStreaming(harness, false);
		harness.session.clearQueue();
	});

	it("wakes an idle session and returns after external message admission", async () => {
		const gate = gatedHook({ prompt: "wake now" });
		const harness = await createHarness({ extensionFactories: [gate.factory] });
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("done")]);

		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", {
				message: "wake now",
				key: "matrix:$idle",
			}),
		).resolves.toEqual({ accepted: true, deliveryStatus: "delivered", key: "matrix:$idle" });
		await gate.reached;
		expect(harness.session.isStreaming).toBe(false);
		expect(harness.session.queuedActionCount).toBe(0);
		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", {
				message: "duplicate while the admitted action is preparing",
				key: "matrix:$idle",
			}),
		).resolves.toMatchObject({ accepted: true, deliveryStatus: "coalesced" });

		gate.release();
		await harness.session.waitForIdle();
		expect(getUserTexts(harness)).toEqual(["wake now"]);
	});

	it("releases external event keys after completion and cancellation", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("first done"), fauxAssistantMessage("second done")]);

		await harness.session.handleSessionMessageHostRequest("session_message.send", {
			message: "first event",
			key: "matrix:$reuse",
		});
		await harness.session.waitForIdle();
		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", {
				message: "same key after completion",
				key: "matrix:$reuse",
			}),
		).resolves.toMatchObject({ accepted: true, deliveryStatus: "delivered" });
		await harness.session.waitForIdle();

		withStreaming(harness, true);
		await harness.session.handleSessionMessageHostRequest("session_message.send", {
			message: "cancel this event",
			key: "matrix:$cancelled",
		});
		expect(harness.session.clearQueue().steering).toEqual(["cancel this event"]);
		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", {
				message: "same key after cancellation",
				key: "matrix:$cancelled",
			}),
		).resolves.toMatchObject({ accepted: true, deliveryStatus: "queued" });
		expect(harness.session.getSteeringMessages()).toEqual(["same key after cancellation"]);
		withStreaming(harness, false);
		harness.session.clearQueue();
	});

	it("rejects malformed external session messages", async () => {
		const harness = await createHarness();
		harnesses.push(harness);

		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", { message: "ok" }),
		).rejects.toThrow("key must be a string");
		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", { message: " ", key: "event" }),
		).rejects.toThrow("cannot be empty");
		await expect(
			harness.session.handleSessionMessageHostRequest("session_message.send", { message: "ok", key: " " }),
		).rejects.toThrow("key cannot be empty");
	});

	it("gives nextTurn delivery precedence over triggerTurn", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("done")]);

		await harness.session.sendCustomMessage(
			{ customType: "precedence", content: "context only", display: true },
			{ triggerTurn: true, deliverAs: "nextTurn" },
		);

		expect(harness.session.messages).toEqual([]);
		expect(harness.session.queuedActionCount).toBe(0);
		expect(harness.getPendingResponseCount()).toBe(1);

		await harness.session.prompt("consume context");
		expect(harness.session.messages.map((message) => message.role)).toEqual(["custom", "user", "assistant"]);
		expect(harness.getPendingResponseCount()).toBe(0);
	});

	it("restores normalized payloads without interception, parsing, or an idle wake", async () => {
		let inputHandlerRuns = 0;
		let extensionCommandRuns = 0;
		const harness = await createHarness({
			extensionFactories: [
				(pi) => {
					pi.on("input", async () => {
						inputHandlerRuns++;
						return { action: "transform", text: "rewritten" };
					});
					pi.registerCommand("literal", {
						description: "must stay literal",
						handler: async () => {
							extensionCommandRuns++;
						},
					});
				},
			],
		});
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("steer done"), fauxAssistantMessage("follow-up done")]);

		await harness.session.restoreFollowUpMessage("/compact");
		await harness.session.restoreSteeringMessage("/literal keep text");
		await Promise.resolve();

		expect(inputHandlerRuns).toBe(0);
		expect(extensionCommandRuns).toBe(0);
		expect(harness.session.getSteeringMessages()).toEqual(["/literal keep text"]);
		expect(harness.session.getFollowUpMessages()).toEqual(["/compact"]);
		expect(harness.session.messages).toEqual([]);

		expect(harness.session.resumeQueuedWork()).toBe(true);
		await harness.session.waitForIdle();
		expect(getUserTexts(harness)).toEqual(["/literal keep text", "/compact"]);
		expect(inputHandlerRuns).toBe(0);
		expect(extensionCommandRuns).toBe(0);
	});
});
