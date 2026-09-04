import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import {
	createEnglishOutputNudgeMessage,
	ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE,
	ENGLISH_OUTPUT_NUDGE_PROMPT,
	stripChineseOutputBlocks,
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

describe("stripChineseOutputBlocks", () => {
	it("removes text blocks with Chinese and keeps thinking", () => {
		const message = fauxAssistantMessage([
			{ type: "thinking", thinking: "The tests passed." },
			{ type: "text", text: "测试通过了" },
			{ type: "text", text: "Next I will inspect the diff." },
		]);
		const stripped = stripChineseOutputBlocks(message);
		expect(stripped?.content).toEqual([
			{ type: "thinking", thinking: "The tests passed." },
			{ type: "text", text: "Next I will inspect the diff." },
		]);
	});

	it("ignores Chinese in thinking traces", () => {
		const message = fauxAssistantMessage([
			{ type: "thinking", thinking: "先看测试结果" },
			{ type: "text", text: "The tests passed." },
		]);
		expect(stripChineseOutputBlocks(message)).toBeUndefined();
	});
});

describe("convertToLlm english output nudge", () => {
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

	it("drops Chinese assistant text then steers an English reminder", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		harness.setResponses([
			fauxAssistantMessage("完成了，测试通过。"),
			fauxAssistantMessage("The tests passed. I will continue in English."),
		]);

		await harness.session.prompt("status?");
		await harness.session.waitForIdle();

		const texts = harness.session.messages.map((message) => getMessageText(message));
		expect(texts.some((text) => text.includes("完成"))).toBe(false);
		expect(
			harness.session.messages.some(
				(message) => message.role === "custom" && message.customType === ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE,
			),
		).toBe(true);
		expect(texts.some((text) => text.includes("I will continue in English"))).toBe(true);
	});

	it("keeps thinking and English text when stripping a mixed assistant message", async () => {
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
			content: [{ type: "thinking", thinking: "Check the test result." }],
		});
		expect(JSON.stringify(harness.session.messages)).not.toContain("测试");
	});
});
