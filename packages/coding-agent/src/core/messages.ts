/**
 * Custom message types and transformers for the coding agent.
 *
 * Extends the base AgentMessage type with coding-agent specific message types,
 * and provides a transformer to convert them to LLM-compatible messages.
 */

import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { ImageContent, Message, ProviderPayload, TextContent } from "@earendil-works/pi-ai";
import { isReasoningExhaustedResponse } from "@earendil-works/pi-ai";
import type { AgentCronJob } from "./cron-jobs.js";
import { ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE, ENGLISH_OUTPUT_NUDGE_PROMPT } from "./english-output-nudge.js";
import { REASONING_OUTPUT_NUDGE_CUSTOM_TYPE } from "./reasoning-output-nudge.js";
import type { AppliedRefinementEdit, HarnessScope, RefinementResult } from "./refinement/refinement.js";
import { isSessionSlashCommandName, parseSessionSlashCommand, type SessionSlashCommand } from "./slash-commands.js";
import { TOOL_ERROR_NUDGE_CUSTOM_TYPE } from "./tool-error-nudge.js";

export const COMPACTION_SUMMARY_PREFIX = `The conversation history before this point was compacted into the following summary. This summary is background; the newest user message in the conversation after it is the live instruction. Do not re-execute requests the summary records as Done:

<summary>
`;

export const COMPACTION_SUMMARY_SUFFIX = `
</summary>`;

export const BRANCH_SUMMARY_PREFIX = `The following is a summary of a branch that this conversation came back from:

<summary>
`;

export const BRANCH_SUMMARY_SUFFIX = `</summary>`;

export const HEARTBEAT_PROMPT_CUSTOM_TYPE = "heartbeat_prompt";
export const HEARTBEAT_PROMPT_PREVIEW_LABEL = "Heartbeat prompt";
export const REPETITION_NOTICE_CUSTOM_TYPE = "repetition_notice";
export const REPETITION_NOTICE_PREVIEW_LABEL = "Repetition notice";
export const REPETITION_NOTICE_PROMPT =
	"You are repeating yourself, take a step back, remember the bigger picture, collect more information if necessary, and continue.";
export const IPYTHON_STATE_RESTORED_CUSTOM_TYPE = "ipython_state_restored";
export const SESSION_SLASH_COMMAND_CUSTOM_TYPE = "session_slash_command";
export const SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE = "session_slash_command_result";
export const COMPACTION_OUTCOME_CUSTOM_TYPE = "compaction_outcome";
export const REFINEMENT_OUTCOME_CUSTOM_TYPE = "refinement_outcome";
export const RLM_CHILD_FAILURE_CUSTOM_TYPE = "rlm_child_failure";
export const RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE = "rlm_child_terminal_notice";
export const MANUAL_CONTINUE_CUSTOM_TYPE = "manual_continue";
/** Hidden extension context that is intended for the model, not the transcript UI. */
export const MODEL_CONTEXT_CUSTOM_TYPE = "model-context";
export const MANUAL_CONTINUE_PROMPT = `<system-notice>
Continue.

- Resume the most recent intent and carry unfinished work to completion.
- If interrupted mid-step, continue from that point.
- Do not pause to summarize progress, reconfirm the plan, or ask whether to proceed.
</system-notice>`;

export interface SessionSlashCommandDetails {
	command: SessionSlashCommand;
	commandEntryId?: string;
}

export interface SessionSlashCommandResultDetails {
	command: SessionSlashCommand;
	success: boolean;
	severity: "info" | "warning" | "error";
	error?: string;
	commandEntryId?: string;
}

export interface SessionSlashCommandMessage extends CustomMessage<SessionSlashCommandDetails> {
	customType: typeof SESSION_SLASH_COMMAND_CUSTOM_TYPE;
	content: string;
	details: SessionSlashCommandDetails;
}

export interface SessionSlashCommandResultMessage extends CustomMessage<SessionSlashCommandResultDetails> {
	customType: typeof SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE;
	content: string;
	details: SessionSlashCommandResultDetails;
}

export type CompactionOutcomeReason = "threshold" | "overflow" | "requested";
export type CompactionOutcome = "skipped" | "cancelled" | "failed";

export interface CompactionOutcomeDetails {
	reason: CompactionOutcomeReason;
	outcome: CompactionOutcome;
}

export interface CompactionOutcomeMessage extends CustomMessage<CompactionOutcomeDetails> {
	customType: typeof COMPACTION_OUTCOME_CUSTOM_TYPE;
	content: string;
	details: CompactionOutcomeDetails;
}

export interface RefinementOutcomeDetails {
	refinementId: string;
	summary: string;
	scope: HarnessScope;
	rollbackOf?: string;
	edits: AppliedRefinementEdit[];
}

export interface RefinementOutcomeMessage extends CustomMessage<RefinementOutcomeDetails> {
	customType: typeof REFINEMENT_OUTCOME_CUSTOM_TYPE;
	content: string;
	details: RefinementOutcomeDetails;
}

export interface RlmChildFailureDetails {
	childId: string;
	sessionName: string;
	error: string;
}

export type RlmChildTerminalNoticeDetails =
	| {
			kind: "cancelled";
			childId: string;
			sessionName: string;
			reason?: string;
	  }
	| {
			kind: "completed_without_reply";
			childId: string;
			sessionName: string;
			lastAssistantTextPreview?: string;
	  };

export function createRlmChildFailureMessage(
	details: RlmChildFailureDetails,
	timestamp = Date.now(),
): CustomMessage<RlmChildFailureDetails> {
	return {
		role: "custom",
		customType: RLM_CHILD_FAILURE_CUSTOM_TYPE,
		content: `RLM child ${details.sessionName} (${details.childId}) failed: ${details.error}`,
		display: true,
		details,
		timestamp,
	};
}

export function createRlmChildTerminalNoticeMessage(
	details: RlmChildTerminalNoticeDetails,
	timestamp = Date.now(),
): CustomMessage<RlmChildTerminalNoticeDetails> {
	const content =
		details.kind === "cancelled"
			? `RLM child ${details.sessionName} (${details.childId}) was cancelled${details.reason ? `: ${details.reason}` : ""}`
			: `RLM child ${details.sessionName} (${details.childId}) completed without sending a reply${details.lastAssistantTextPreview ? `. Last assistant text: ${details.lastAssistantTextPreview}` : ""}`;
	return {
		role: "custom",
		customType: RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE,
		content,
		display: true,
		details,
		timestamp,
	};
}

/**
 * Message type for bash executions via the ! command.
 */
export interface BashExecutionMessage {
	role: "bashExecution";
	command: string;
	output: string;
	exitCode: number | undefined;
	cancelled: boolean;
	truncated: boolean;
	fullOutputPath?: string;
	timestamp: number;
	/** If true, this message is excluded from LLM context (!! prefix) */
	excludeFromContext?: boolean;
}

/**
 * Message type for extension-injected messages via sendMessage().
 * These are custom messages that extensions can inject into the conversation.
 */
export interface CustomMessage<T = unknown> {
	role: "custom";
	customType: string;
	content: string | (TextContent | ImageContent)[];
	display: boolean;
	details?: T;
	timestamp: number;
}

export interface HeartbeatPromptDetails {
	jobId: string;
	schedule: string;
	status: AgentCronJob["status"];
	runCount: number;
	nextRunAt?: string;
	lastRunAt?: string;
}

export interface IpythonStateRestoredDetails {
	restored: boolean;
}

export interface BranchSummaryMessage {
	role: "branchSummary";
	summary: string;
	fromId: string;
	timestamp: number;
}

export interface CompactionSummaryMessage {
	role: "compactionSummary";
	summary: string;
	tokensBefore: number;
	/** Number of retained messages that precede this summary in transcript presentation. */
	retainedMessageCount?: number;
	/** Provider-native history replayed instead of the display summary by a matching provider. */
	providerPayload?: ProviderPayload;
	/** User instructions that guided the summary (from `/compact <instructions>`) */
	customInstructions?: string;
	timestamp: number;
}

declare module "@earendil-works/pi-agent-core" {
	interface CustomAgentMessages {
		bashExecution: BashExecutionMessage;
		custom: CustomMessage;
		branchSummary: BranchSummaryMessage;
		compactionSummary: CompactionSummaryMessage;
	}
}

/**
 * Format bash output for LLM context. The fence must be longer than any
 * backtick run in the output so command output cannot terminate it early.
 */
export function bashOutputToText(
	msg: Pick<BashExecutionMessage, "output" | "exitCode" | "cancelled" | "truncated" | "fullOutputPath">,
): string {
	let text = "";
	if (msg.output) {
		let longestBacktickRun = 0;
		for (const match of msg.output.matchAll(/`+/g)) {
			longestBacktickRun = Math.max(longestBacktickRun, match[0].length);
		}
		const fence = "`".repeat(Math.max(3, longestBacktickRun + 1));
		text += `${fence}\n${msg.output}\n${fence}`;
	} else {
		text += "(no output)";
	}
	if (msg.cancelled) {
		text += "\n\n(command cancelled)";
	} else if (msg.exitCode !== null && msg.exitCode !== undefined && msg.exitCode !== 0) {
		text += `\n\nCommand exited with code ${msg.exitCode}`;
	}
	if (msg.truncated) {
		text += msg.fullOutputPath
			? `\n\n[Output truncated. Full output: ${msg.fullOutputPath}]`
			: "\n\n[Output truncated.]";
	}
	return text;
}

/**
 * Convert a BashExecutionMessage to user message text for LLM context.
 */
export function bashExecutionToText(msg: BashExecutionMessage): string {
	return `Ran \`${msg.command}\`\n${bashOutputToText(msg)}`;
}

export function createBranchSummaryMessage(summary: string, fromId: string, timestamp: string): BranchSummaryMessage {
	return {
		role: "branchSummary",
		summary,
		fromId,
		timestamp: new Date(timestamp).getTime(),
	};
}

export function createCompactionSummaryMessage(
	summary: string,
	tokensBefore: number,
	timestamp: string,
	customInstructions?: string,
	retainedMessageCount?: number,
	providerPayload?: ProviderPayload,
): CompactionSummaryMessage {
	return {
		role: "compactionSummary",
		summary,
		tokensBefore,
		retainedMessageCount,
		providerPayload,
		customInstructions,
		timestamp: new Date(timestamp).getTime(),
	};
}

/** Convert CustomMessageEntry to AgentMessage format */
export function createCustomMessage(
	customType: string,
	content: string | (TextContent | ImageContent)[],
	display: boolean,
	details: unknown | undefined,
	timestamp: string,
): CustomMessage {
	return {
		role: "custom",
		customType,
		content,
		display,
		details,
		timestamp: new Date(timestamp).getTime(),
	};
}

export function createManualContinueMessage(timestamp = Date.now()): CustomMessage {
	return {
		role: "custom",
		customType: MANUAL_CONTINUE_CUSTOM_TYPE,
		content: MANUAL_CONTINUE_PROMPT,
		display: false,
		timestamp,
	};
}

export function createSessionSlashCommandMessage(
	command: SessionSlashCommand,
	details: Omit<SessionSlashCommandDetails, "command"> = {},
	display = true,
	timestamp = Date.now(),
): SessionSlashCommandMessage {
	return {
		role: "custom",
		customType: SESSION_SLASH_COMMAND_CUSTOM_TYPE,
		content: command.text,
		display,
		details: { ...details, command: { ...command } },
		timestamp,
	};
}

export function createSessionSlashCommandResultMessage(
	content: string,
	details: SessionSlashCommandResultDetails,
	display = true,
	timestamp = Date.now(),
): SessionSlashCommandResultMessage {
	return {
		role: "custom",
		customType: SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE,
		content,
		display,
		details: { ...details, command: { ...details.command } },
		timestamp,
	};
}

export function createCompactionOutcomeMessage(
	content: string,
	details: CompactionOutcomeDetails,
	display = true,
	timestamp = Date.now(),
): CompactionOutcomeMessage {
	return {
		role: "custom",
		customType: COMPACTION_OUTCOME_CUSTOM_TYPE,
		content,
		display,
		details: { ...details },
		timestamp,
	};
}

export function createRefinementOutcomeMessage(
	result: RefinementResult,
	display = true,
	timestamp = Date.now(),
): RefinementOutcomeMessage {
	return {
		role: "custom",
		customType: REFINEMENT_OUTCOME_CUSTOM_TYPE,
		content: `Refinement complete: ${result.summary}`,
		display,
		details: {
			refinementId: result.id,
			summary: result.summary,
			scope: result.scope ?? "local",
			...(result.rollbackOf ? { rollbackOf: result.rollbackOf } : {}),
			edits: result.appliedEdits,
		},
		timestamp,
	};
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null;
}

function hasValidCustomMessageEnvelope(message: Record<string, unknown>, customType: string): boolean {
	return (
		message.role === "custom" &&
		message.customType === customType &&
		typeof message.content === "string" &&
		typeof message.display === "boolean" &&
		typeof message.timestamp === "number" &&
		Number.isFinite(message.timestamp)
	);
}

export function isSessionSlashCommand(value: unknown): value is SessionSlashCommand {
	if (
		!isRecord(value) ||
		!isSessionSlashCommandName(value.name) ||
		typeof value.args !== "string" ||
		typeof value.text !== "string"
	) {
		return false;
	}
	const parsed = parseSessionSlashCommand(value.text);
	return (
		parsed !== undefined && parsed.name === value.name && parsed.args === value.args && parsed.text === value.text
	);
}

function isValidCommandEntryId(value: unknown): value is string | undefined {
	return value === undefined || (typeof value === "string" && value.length > 0);
}

export function isSessionSlashCommandMessage(message: unknown): message is SessionSlashCommandMessage {
	if (
		!isRecord(message) ||
		!hasValidCustomMessageEnvelope(message, SESSION_SLASH_COMMAND_CUSTOM_TYPE) ||
		typeof message.content !== "string"
	) {
		return false;
	}
	if (!isRecord(message.details) || !isSessionSlashCommand(message.details.command)) return false;
	return message.content === message.details.command.text && isValidCommandEntryId(message.details.commandEntryId);
}

export function isSessionSlashCommandResultMessage(message: unknown): message is SessionSlashCommandResultMessage {
	if (!isRecord(message) || !hasValidCustomMessageEnvelope(message, SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE))
		return false;
	if (!isRecord(message.details) || !isSessionSlashCommand(message.details.command)) return false;
	return (
		typeof message.details.success === "boolean" &&
		(message.details.severity === "info" ||
			message.details.severity === "warning" ||
			message.details.severity === "error") &&
		(message.details.error === undefined || typeof message.details.error === "string") &&
		isValidCommandEntryId(message.details.commandEntryId)
	);
}

export function isCompactionOutcomeMessage(message: unknown): message is CompactionOutcomeMessage {
	if (!isRecord(message) || !hasValidCustomMessageEnvelope(message, COMPACTION_OUTCOME_CUSTOM_TYPE)) return false;
	if (!isRecord(message.details)) return false;
	return (
		(message.details.reason === "threshold" ||
			message.details.reason === "overflow" ||
			message.details.reason === "requested") &&
		(message.details.outcome === "skipped" ||
			message.details.outcome === "cancelled" ||
			message.details.outcome === "failed")
	);
}

function isAppliedRefinementEdit(value: unknown): value is AppliedRefinementEdit {
	return (
		isRecord(value) &&
		(value.action === "create" || value.action === "update" || value.action === "delete") &&
		typeof value.kind === "string" &&
		typeof value.id === "string" &&
		typeof value.applied === "boolean"
	);
}

export function isRefinementOutcomeMessage(message: unknown): message is RefinementOutcomeMessage {
	if (!isRecord(message) || !hasValidCustomMessageEnvelope(message, REFINEMENT_OUTCOME_CUSTOM_TYPE)) return false;
	if (!isRecord(message.details)) return false;
	return (
		typeof message.details.summary === "string" &&
		(message.details.scope === "local" || message.details.scope === "global") &&
		Array.isArray(message.details.edits) &&
		message.details.edits.every(isAppliedRefinementEdit)
	);
}

export function createHeartbeatPromptMessage(
	job: AgentCronJob,
	timestamp = Date.now(),
): CustomMessage<HeartbeatPromptDetails> {
	return {
		role: "custom",
		customType: HEARTBEAT_PROMPT_CUSTOM_TYPE,
		content: job.prompt,
		display: true,
		details: {
			jobId: job.id,
			schedule: job.schedule.expression,
			status: job.status,
			runCount: job.runCount,
			nextRunAt: job.nextRunAt,
			lastRunAt: job.lastRunAt,
		},
		timestamp,
	};
}

export function createRepetitionNoticeMessage(timestamp = Date.now()): CustomMessage {
	return {
		role: "custom",
		customType: REPETITION_NOTICE_CUSTOM_TYPE,
		content: REPETITION_NOTICE_PROMPT,
		display: true,
		timestamp,
	};
}

function customMessageText(message: CustomMessage): string {
	return typeof message.content === "string"
		? message.content
		: message.content.map((block) => (block.type === "text" ? block.text : "")).join("");
}

function isRepetitionLoopAssistant(message: AgentMessage): boolean {
	return (
		message.role === "assistant" &&
		(message.diagnostics?.some((diagnostic) => diagnostic.type === "agent_repetition_loop") ?? false)
	);
}

const ELAPSED_TIME_INTERVAL_SECONDS = 30;

/**
 * addElapsedSystemPrompt exposes quantized session time only to the provider.
 *
 * Keeping the hint in the transient system prompt avoids transcript and TUI
 * messages. Quantization also keeps the prompt stable between 30-second
 * boundaries.
 */
export function addElapsedSystemPrompt(
	systemPrompt: string | undefined,
	conversationStartedAt: number,
	now = Date.now(),
): string | undefined {
	if (!Number.isFinite(conversationStartedAt) || !Number.isFinite(now)) return systemPrompt;
	const elapsedSeconds = Math.max(0, Math.floor((now - conversationStartedAt) / 1000));
	const elapsedBucket = Math.floor(elapsedSeconds / ELAPSED_TIME_INTERVAL_SECONDS) * ELAPSED_TIME_INTERVAL_SECONDS;
	if (elapsedBucket < ELAPSED_TIME_INTERVAL_SECONDS) return systemPrompt;
	const hint = `<session-elapsed-time>
T+${elapsedBucket}s since the first persisted session message.
</session-elapsed-time>`;
	return systemPrompt && systemPrompt.length > 0
		? `${systemPrompt}

${hint}`
		: hint;
}

/**
 * Transform AgentMessages (including custom types) to LLM-compatible Messages.
 *
 * This is used by:
 * - Agent's transormToLlm option (for prompt calls and queued messages)
 * - Compaction's generateSummary (for summarization)
 * - Custom extensions and tools
 */
export function convertToLlm(messages: AgentMessage[]): Message[] {
	return messages
		.map((m): Message | undefined => {
			switch (m.role) {
				case "bashExecution":
					if (m.excludeFromContext) {
						return undefined;
					}
					return {
						role: "user",
						content: [{ type: "text", text: bashExecutionToText(m) }],
						timestamp: m.timestamp,
					};
				case "custom": {
					if (m.customType === MANUAL_CONTINUE_CUSTOM_TYPE) {
						return {
							role: "user",
							content: [{ type: "text", text: MANUAL_CONTINUE_PROMPT }],
							timestamp: m.timestamp,
						};
					}
					if (
						m.customType === REPETITION_NOTICE_CUSTOM_TYPE ||
						m.customType === TOOL_ERROR_NUDGE_CUSTOM_TYPE ||
						m.customType === REASONING_OUTPUT_NUDGE_CUSTOM_TYPE ||
						m.customType === ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE
					) {
						return {
							role: "user",
							content: [
								{
									type: "text",
									text: `<system-notice>\n${m.customType === ENGLISH_OUTPUT_NUDGE_CUSTOM_TYPE ? ENGLISH_OUTPUT_NUDGE_PROMPT : customMessageText(m)}\n</system-notice>`,
								},
							],
							timestamp: m.timestamp,
						};
					}
					if (
						(!m.display && m.customType !== MODEL_CONTEXT_CUSTOM_TYPE) ||
						m.customType === SESSION_SLASH_COMMAND_CUSTOM_TYPE ||
						m.customType === SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE ||
						m.customType === COMPACTION_OUTCOME_CUSTOM_TYPE ||
						m.customType === REFINEMENT_OUTCOME_CUSTOM_TYPE
					) {
						return undefined;
					}
					const content = typeof m.content === "string" ? [{ type: "text" as const, text: m.content }] : m.content;
					return {
						role: "user",
						content,
						timestamp: m.timestamp,
					};
				}
				case "branchSummary":
					return {
						role: "user",
						content: [{ type: "text" as const, text: BRANCH_SUMMARY_PREFIX + m.summary + BRANCH_SUMMARY_SUFFIX }],
						timestamp: m.timestamp,
					};
				case "compactionSummary":
					return {
						role: "user",
						content: [
							{ type: "text" as const, text: COMPACTION_SUMMARY_PREFIX + m.summary + COMPACTION_SUMMARY_SUFFIX },
						],
						providerPayload: m.providerPayload,
						timestamp: m.timestamp,
					};
				case "assistant":
					if (isRepetitionLoopAssistant(m) || isReasoningExhaustedResponse(m)) return undefined;
					return m;
				case "user":
				case "toolResult":
					return m;
				default:
					// biome-ignore lint/correctness/noSwitchDeclarations: fine
					const _exhaustiveCheck: never = m;
					return undefined;
			}
		})
		.filter((m) => m !== undefined);
}
