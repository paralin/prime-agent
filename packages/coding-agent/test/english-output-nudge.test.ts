import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, describe, expect, it } from "vitest";
import {
	createEnglishOutputNudgeMessage,
	ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE,
	ENGLISH_OUTPUT_NUDGE_PROMPT,
	needsEnglishOutputNudge,
	textHasChinese,
} from "../src/core/english-output-nudge.js";
import { convertToLlm } from "../src/core/messages.js";
import { createHarness, getMessageText, type Harness } from "./suite/harness.js";

describe("textHasChinese", () => {
	it("detects Han ideographs and ignores Latin, hiragana, and hangul", () => {
		expect(textHasChinese("完成了")).toBe(true);
		expect(textHasChinese("The test passed.")).toBe(false);
		expect(textHasChinese("こんにちは")).toBe(false);
		expect(textHasChinese("안녕하세요")).toBe(false);
	});

	it("ignores ASCII art with a couple of box-drawing or stray CJK characters", () => {
		// ASCII art with box-drawing characters and only one or two stray Han chars
		expect(textHasChinese("┌─┬─┐\n│ ├─┤ │\n└─┴─┘")).toBe(false);
		expect(textHasChinese("+----+\n| 门 |\n+----+")).toBe(false);
		expect(textHasChinese("schema ──> parser ──> renderer")).toBe(false);
		expect(textHasChinese("棋")).toBe(false);
		expect(textHasChinese("")).toBe(false);
	});

	it("detects genuine Chinese sentences even when mixed with ASCII", () => {
		expect(textHasChinese("测试通过了，所有用例都跑完了。")).toBe(true);
		expect(textHasChinese("Run npx vitest run, 然后检查输出。")).toBe(true);
	});
});

describe("needsEnglishOutputNudge", () => {
	it("detects Chinese explanations without mutating the message", () => {
		const message = fauxAssistantMessage([
			{ type: "thinking", thinking: "The tests passed." },
			{ type: "text", text: "测试通过了" },
			{ type: "text", text: "Next I will inspect the diff." },
		]);
		expect(needsEnglishOutputNudge(message)).toBe(true);
		expect(message.content).toEqual([
			{ type: "thinking", thinking: "The tests passed." },
			{ type: "text", text: "测试通过了" },
			{ type: "text", text: "Next I will inspect the diff." },
		]);
	});

	it("detects Chinese in thinking traces", () => {
		const message = fauxAssistantMessage([
			{ type: "thinking", thinking: "先看测试结果" },
			{ type: "text", text: "The tests passed." },
		]);
		expect(needsEnglishOutputNudge(message)).toBe(true);
	});

	it("checks Python code lines without modifying arguments or scanning unrelated tools", () => {
		const code = `${"value = 1\n".repeat(40)}# 接下来检查结果\nprint(value)`;
		const message = fauxAssistantMessage([{ type: "toolCall", id: "python", name: "ipython", arguments: { code } }]);
		expect(needsEnglishOutputNudge(message)).toBe(true);
		expect(message.content[0]).toMatchObject({ arguments: { code } });
		expect(
			needsEnglishOutputNudge(
				fauxAssistantMessage([{ type: "toolCall", id: "read", name: "read", arguments: { path: "中文文件.txt" } }]),
			),
		).toBe(false);
	});
});

describe("convertToLlm english output nudge", () => {
	it("replaces historical rewrite notices only at the model boundary", () => {
		const notice = {
			...createEnglishOutputNudgeMessage(10),
			content: "Rewrite your previous response and reasoning.",
		};
		const converted = convertToLlm([notice]);
		expect(JSON.stringify(converted)).toContain(ENGLISH_OUTPUT_NUDGE_PROMPT);
		expect(JSON.stringify(converted)).not.toContain("Rewrite your previous response");
		expect(notice.content).toBe("Rewrite your previous response and reasoning.");
	});
	it("sends the reminder as a system-notice user turn", () => {
		const notice = createEnglishOutputNudgeMessage(10);
		expect(convertToLlm([notice])).toEqual([
			{
				role: "user",
				content: [{ type: "text", text: `<system-notice>\n${ENGLISH_OUTPUT_NUDGE_PROMPT}\n</system-notice>` }],
				timestamp: 10,
			},
		]);
	});
});

describe("AgentSession english output nudge", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
	});

	it("executes Python once and queues the reminder after the result without rewriting code", async () => {
		const harness = await createHarness({ tools: [] });
		harnesses.push(harness);
		let calls = 0;
		const code = '# 检查测试结果\nprint("ok")';
		harness.session.agent.state.tools = [
			{
				name: "ipython",
				label: "ipython",
				description: "Fixture Python",
				parameters: Type.Object({ code: Type.String() }),
				execute: async (_id, args) => {
					calls++;
					expect(args).toEqual({ code });
					return { content: [{ type: "text", text: "ok" }], details: {} };
				},
			},
		];
		harness.setResponses([
			fauxAssistantMessage([{ type: "toolCall", id: "python", name: "ipython", arguments: { code } }], {
				stopReason: "toolUse",
			}),
			fauxAssistantMessage("Continuing in English."),
			fauxAssistantMessage("Done."),
		]);
		await harness.session.prompt("Check it");
		await harness.session.waitForHeadlessIdle();
		expect(calls).toBe(1);
		expect(JSON.stringify(harness.sessionManager.getEntries())).toContain("检查测试结果");
		expect(
			harness.session.messages.some(
				(message) => message.role === "custom" && message.customType === ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE,
			),
		).toBe(true);
	});

	it("preserves Chinese assistant text then steers a prospective English reminder", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		harness.setResponses([
			fauxAssistantMessage("完成了，测试通过。"),
			(context) => {
				expect(
					context.messages.some((message) => JSON.stringify(message.content).includes("完成了，测试通过。")),
				).toBe(true);
				expect(JSON.stringify(context.messages.at(-1)?.content)).toContain(ENGLISH_OUTPUT_NUDGE_PROMPT);
				return fauxAssistantMessage("The tests passed. I will continue in English.");
			},
		]);

		await harness.session.prompt("status?");
		await harness.session.waitForIdle();

		const texts = harness.session.messages.map((message) => getMessageText(message));
		expect(texts.some((text) => text.includes("完成"))).toBe(true);
		expect(JSON.stringify(harness.sessionManager.getEntries())).toContain("完成了，测试通过。");
		expect(
			harness.session.messages.some(
				(message) => message.role === "custom" && message.customType === ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE,
			),
		).toBe(true);
		expect(texts.some((text) => text.includes("I will continue in English"))).toBe(true);
	});

	it("preserves thinking and text in a mixed assistant message", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		harness.setResponses([
			fauxAssistantMessage([
				{ type: "thinking", thinking: "Check the test result." },
				{ type: "text", text: "测试通过了" },
			]),
			fauxAssistantMessage("Continuing."),
		]);

		await harness.session.prompt("status?");
		await harness.session.waitForIdle();

		const assistant = harness.session.messages.find((message) => message.role === "assistant");
		expect(assistant).toMatchObject({
			content: [
				{ type: "thinking", thinking: "Check the test result." },
				{ type: "text", text: "测试通过了" },
			],
		});
		expect(JSON.stringify(harness.session.messages)).toContain("测试");
	});
});
