import type { AssistantMessage } from "@earendil-works/pi-ai";
import type { CustomMessage } from "./messages.js";

export const ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE = "english_output_nudge";
export const ENGLISH_OUTPUT_NUDGE_PREVIEW_LABEL = "English reminder";
export const ENGLISH_OUTPUT_NUDGE_PROMPT =
	"Continue the user's active task from the latest tool result. Use English for subsequent reasoning, explanations, and Python code comments and prose. Preserve quoted source data, exact paths, and required non-English strings. This is a language reminder, not a new task: do not reconstruct the conversation or repeat completed work. No reply to this notice is needed.";

// Han ideographs in assistant-authored content, never tool-result data.
// Density-based: a couple of stray Han characters (or Han glyphs inside ASCII art)
// must not flag the block, so require at least 2 Han characters and at least 5% of
// all characters in the block.
export function textHasChinese(text: string): boolean {
	const chars = [...text];
	if (chars.length === 0) return false;
	const han = chars.filter((c) => /\p{Unified_Ideograph}/u.test(c)).length;
	return han >= 2 && han / chars.length >= 0.05;
}

/** needsEnglishOutputNudge checks assistant prose and Python code without changing the transcript. */
export function needsEnglishOutputNudge(message: AssistantMessage): boolean {
	return message.content.some((block) => {
		if (block.type === "text") return textHasChinese(block.text);
		if (block.type === "thinking") return textHasChinese(block.thinking);
		return (
			block.type === "toolCall" &&
			block.name === "ipython" &&
			typeof block.arguments.code === "string" &&
			block.arguments.code.split("\n").some(textHasChinese)
		);
	});
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
