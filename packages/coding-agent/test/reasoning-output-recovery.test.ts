import { fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, describe, expect, it } from "vitest";
import { convertToLlm } from "../src/core/messages.js";
import {
	createReasoningOutputNudgeMessage,
	REASONING_OUTPUT_NUDGE_CUSTOM_TYPE,
} from "../src/core/reasoning-output-nudge.js";
import {
	InjectedPromptMessageComponent,
	isInjectedPromptMessage,
} from "../src/modes/interactive/components/injected-prompt-message.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";
import { createHarness, type Harness } from "./suite/harness.js";

function exhausted() {
	const message = fauxAssistantMessage([{ type: "thinking", thinking: "failed deliberation must not replay" }], {
		stopReason: "length",
	});
	message.diagnostics = [
		{ type: "provider_warning", timestamp: 0, error: { code: "reasoning_exhausted", message: "No answer" } },
	];
	return message;
}

describe("reasoning output recovery", () => {
	const harnesses: Harness[] = [];
	it.each([40, 80, 120])("renders an expandable recovery notice at width %s", (width) => {
		initTheme("dark");
		const message = createReasoningOutputNudgeMessage();
		expect(isInjectedPromptMessage(message)).toBe(true);
		const component = new InjectedPromptMessageComponent(message);
		expect(component.render(width).join("\n")).toContain("Reasoning recovery");
		component.setExpanded(true);
		expect(component.render(width).join("\n")).toContain("Collect more information");
	});
	afterEach(() => {
		for (const harness of harnesses.splice(0)) harness.cleanup();
	});

	it.each([false, true])("resumes with the exact guidance, compaction enabled: %s", async (enabled) => {
		const harness = await createHarness({
			settings: {
				compaction: { enabled, strategy: "scratch-handoff", triggerContextTokens: 200000 },
				retry: { enabled: false },
			},
		});
		harnesses.push(harness);
		let originalSystemPrompt = "";
		harness.setResponses([
			(context) => {
				originalSystemPrompt = context.systemPrompt ?? "";
				return exhausted();
			},
			(context) => {
				expect(context.systemPrompt).toBe(originalSystemPrompt);
				expect(context.messages[0]).toMatchObject({
					role: "user",
					content: [{ type: "text", text: "Check the boundary" }],
				});
				expect(JSON.stringify(context.messages)).not.toContain("failed deliberation must not replay");
				expect(context.messages.at(-1)).toMatchObject({
					role: "user",
					content: [
						{
							type: "text",
							text: "<system-notice>\nCollect more information or decide how to combine together existing information you collected before you try to think through what to do next. Once you are better informed you can decide what the next step is.\n</system-notice>",
						},
					],
				});
				return fauxAssistantMessage("The existing evidence establishes the next step.");
			},
		]);
		await harness.session.prompt("Check the boundary");
		await harness.session.waitForHeadlessIdle();
		expect(harness.faux.state.callCount).toBe(2);
		expect(harness.session.messages.at(-1)).toMatchObject({
			role: "assistant",
			stopReason: "stop",
			content: [{ type: "text", text: "The existing evidence establishes the next step." }],
		});
		expect(harness.eventsOfType("compaction_start")).toHaveLength(0);
		expect(harness.eventsOfType("auto_retry_start")).toHaveLength(0);
		expect(JSON.stringify(harness.sessionManager.getEntries())).toContain("failed deliberation must not replay");
		expect(JSON.stringify(convertToLlm(harness.sessionManager.buildSessionContext().messages))).not.toContain(
			"failed deliberation must not replay",
		);
		expect(harness.session.isStreaming).toBe(false);
		expect(harness.session.hasPendingSessionWork).toBe(false);
	});

	it.each(["none", "success", "error", "child"])("bounds recovery with %s progress", async (progress) => {
		const harness = await createHarness({
			settings: { compaction: { enabled: false }, retry: { enabled: false } },
			tools: [
				{
					name: "check_boundary",
					label: "Check boundary",
					description: "Check the boundary.",
					parameters: Type.Object({}),
					execute: async () => {
						if (progress === "error") throw new Error("Boundary unavailable");
						return { content: [{ type: "text", text: "Boundary verified" }], details: {} };
					},
				},
			],
		});
		harnesses.push(harness);
		harness.setResponses([
			exhausted(),
			async () => {
				if (progress === "child") {
					await harness.session.sendCustomMessage(
						{ customType: "agent_message", content: "Child boundary verified", display: true },
						{ triggerTurn: true, deliverAs: "steer" },
					);
				}
				return progress === "success" || progress === "error"
					? fauxAssistantMessage([fauxToolCall("check_boundary", {})], { stopReason: "toolUse" })
					: exhausted();
			},
			exhausted(),
			fauxAssistantMessage("Task complete"),
		]);
		await harness.session.prompt("Check the boundary");
		await harness.session.waitForHeadlessIdle();
		expect(harness.faux.state.callCount).toBe(progress === "success" ? 4 : progress === "none" ? 2 : 3);
		expect(
			harness.sessionManager
				.getEntries()
				.filter(
					(entry) => entry.type === "custom_message" && entry.customType === REASONING_OUTPUT_NUDGE_CUSTOM_TYPE,
				),
		).toHaveLength(progress === "success" ? 2 : 1);
		expect(harness.eventsOfType("compaction_start")).toHaveLength(0);
		expect(harness.session.hasPendingSessionWork).toBe(false);
		expect(harness.session.isStreaming).toBe(false);
	});

	it.each(["length", "error"] as const)(
		"preserves useful content when excluding %s exhaustion on replay",
		(stopReason) => {
			const prior = fauxAssistantMessage([
				{ type: "thinking", thinking: "Useful prior reasoning" },
				{ type: "text", text: "Observed result" },
			]);
			const failed = { ...exhausted(), stopReason };
			expect(convertToLlm([prior, failed])).toEqual([prior]);
			const partial = { ...failed, content: [{ type: "text" as const, text: "Useful partial answer" }] };
			const tool = { ...failed, content: [fauxToolCall("check_boundary", {})] };
			expect(convertToLlm([partial, tool])).toEqual([partial, tool]);
		},
	);
});
