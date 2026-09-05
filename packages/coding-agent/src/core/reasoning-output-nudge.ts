import type { CustomMessage } from "./messages.js";

export const REASONING_OUTPUT_NUDGE_CUSTOM_TYPE = "reasoning_output_nudge";
export const REASONING_OUTPUT_NUDGE_PREVIEW_LABEL = "Reasoning recovery";
export const REASONING_OUTPUT_NUDGE_PROMPT =
	"Collect more information or decide how to combine together existing information you collected before you try to think through what to do next. Once you are better informed you can decide what the next step is.";

/** createReasoningOutputNudgeMessage resumes work after an exhausted reasoning response. */
export function createReasoningOutputNudgeMessage(timestamp = Date.now()): CustomMessage {
	return {
		role: "custom",
		customType: REASONING_OUTPUT_NUDGE_CUSTOM_TYPE,
		content: REASONING_OUTPUT_NUDGE_PROMPT,
		display: true,
		timestamp,
	};
}
