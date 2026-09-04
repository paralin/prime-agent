import type { AssistantMessage } from "@earendil-works/pi-ai";
import type { CustomMessage } from "./messages.js";

export const ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE = "english_output_nudge";
export const ENGLISH_OUTPUT_NUDGE_PREVIEW_LABEL = "English reminder";
export const ENGLISH_OUTPUT_NUDGE_PROMPT =
	"Your previous response contained non-English text, which violates the language policy. Rewrite your response entirely in English for all communication, code, and reasoning. Do not mention this notice.";

// Han ideographs. Thinking traces are ignored; only assistant text output is checked.
// Density-based: a couple of stray Han characters (or Han glyphs inside ASCII art)
// must not flag the block, so require at least 2 Han characters and at least 5% of
// all characters in the block.
export function textHasChinese(text: string): boolean {
	const chars = [...text];
	if (chars.length === 0) return false;
	const han = chars.filter((c) => /\p{Unified_Ideograph}/u.test(c)).length;
	return han >= 2 && han / chars.length >= 0.05;
}

/** stripChineseOutputBlocks removes assistant text blocks that contain Chinese, leaving thinking and tool calls. */
export function stripChineseOutputBlocks(message: AssistantMessage): AssistantMessage | undefined {
	if (message.role !== "assistant") return undefined;
	const content = message.content.filter((block) => block.type !== "text" || !textHasChinese(block.text));
	if (content.length === message.content.length) {
		return undefined;
	}
	return { ...message, content };
}

export function createEnglishOutputNudgeMessage(timestamp = Date.now()): CustomMessage {
	return {
		role: "custom",
		customType: ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE,
		content: ENGLISH_OUTPUT_NUDGE_PROMPT,
		display: true,
		timestamp,
	};
}
