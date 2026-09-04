import type { AssistantMessage } from "@earendil-works/pi-ai";
import type { CustomMessage } from "./messages.js";

export const ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE = "english_output_nudge";
export const ENGLISH_OUTPUT_NUDGE_PREVIEW_LABEL = "English reminder";
export const ENGLISH_OUTPUT_NUDGE_PROMPT =
	"Continue the user's active task from the latest tool result. Use English for subsequent user-facing explanations. This is a language reminder, not a new task: do not reconstruct the conversation or repeat completed work. No reply to this notice is needed.";

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

/** needsEnglishOutputNudge checks visible explanations without changing the transcript. */
export function needsEnglishOutputNudge(message: AssistantMessage): boolean {
	return message.content.some((block) => block.type === "text" && textHasChinese(block.text));
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
