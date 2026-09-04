/**
 * Agent loop that works with AgentMessage throughout.
 * Transforms to Message[] only at the LLM call boundary.
 */

import {
	type AssistantMessage,
	type AssistantMessageEvent,
	type Context,
	createAssistantMessageDiagnostic,
	EventStream,
	getLogger,
	streamSimple,
	type ToolResultMessage,
	validateToolArguments,
} from "@earendil-works/pi-ai";
import { DEFAULT_REPETITION_THRESHOLD, RepetitionDetector } from "./repetition-detector.js";
import type {
	AgentContext,
	AgentEvent,
	AgentLoopConfig,
	AgentMessage,
	AgentTool,
	AgentToolCall,
	AgentToolResult,
	StreamFn,
} from "./types.js";

export type AgentEventSink = (event: AgentEvent) => Promise<void> | void;

const ABORT_ERROR_MESSAGE = "Request was aborted";
// Providers occasionally close a stream before producing final content: without
// a finish reason, with an empty "stop" response, or after spending the output
// limit entirely on thinking. Continuing the same turn usually lets the model
// finish, but a provider that never finishes would loop forever, so consecutive
// incomplete responses are capped.
const MAX_CONSECUTIVE_INCOMPLETE_RESPONSES = 3;
const MAX_CONSECUTIVE_IDENTICAL_TOOL_CALL_BATCHES = 3;
// Providers sometimes accept a request and then deliver no stream events at all:
// a hung gateway or a silently dropped connection leaves the socket open while
// the model produces nothing. Abort the request and retry after this window
// without model output.
const STREAM_STALL_TIMEOUT_MS = 180_000;
// Some providers degenerate mid-stream: the model repeats one word, sentence,
// paragraph, or thought-trace block indefinitely. Detect the loop while the
// stream runs, cancel the provider request, and end the turn with an
// `agent_repetition_loop` diagnostic so the session can recover instead of
// retrying into the same loop.
function repetitionLoopThreshold(config: AgentLoopConfig): number {
	if (config.repetitionLoop?.enabled === false) return 0;
	return config.repetitionLoop?.threshold ?? DEFAULT_REPETITION_THRESHOLD;
}

function finalizeRepetitionLoopMessage(
	config: AgentLoopConfig,
	partialMessage: AssistantMessage | null,
	threshold: number,
): AssistantMessage {
	const errorMessage = `Repetition loop detected: the model repeated the same output ${threshold}+ times; the provider request was cancelled`;
	return {
		role: "assistant",
		content: partialMessage ? cloneAssistantContent(partialMessage.content) : [{ type: "text", text: "" }],
		api: partialMessage?.api ?? config.model.api,
		provider: partialMessage?.provider ?? config.model.provider,
		model: partialMessage?.model ?? config.model.id,
		usage: cloneUsage(partialMessage?.usage ?? EMPTY_USAGE),
		stopReason: "error",
		errorMessage,
		diagnostics: [createAssistantMessageDiagnostic("agent_repetition_loop", new Error(errorMessage), { threshold })],
		timestamp: Date.now(),
	};
}

function canonicalToolArguments(value: unknown): string {
	if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
	if (Array.isArray(value)) {
		return `[${value.map((item) => (item === undefined ? "null" : canonicalToolArguments(item))).join(",")}]`;
	}
	const record = value as Record<string, unknown>;
	return `{${Object.keys(record)
		.filter((key) => record[key] !== undefined)
		.sort()
		.map((key) => `${JSON.stringify(key)}:${canonicalToolArguments(record[key])}`)
		.join(",")}}`;
}

function toolCallBatchSignature(message: AssistantMessage): string | undefined {
	const toolCalls = message.content.filter((content) => content.type === "toolCall");
	if (toolCalls.length === 0) return undefined;
	return toolCalls
		.map((call) => `${call.name}:${canonicalToolArguments(call.arguments)}`)
		.sort()
		.join("\n");
}

class ToolCallRepetitionDetector {
	private lastSignature: string | undefined;
	private lastResultSignature: string | undefined;
	private consecutiveBatches = 0;

	observe(message: AssistantMessage): boolean {
		const signature = toolCallBatchSignature(message);
		if (!signature) {
			this.lastSignature = undefined;
			this.consecutiveBatches = 0;
			return false;
		}
		if (signature === this.lastSignature) {
			this.consecutiveBatches += 1;
		} else {
			this.lastSignature = signature;
			this.consecutiveBatches = 1;
		}
		return this.consecutiveBatches >= MAX_CONSECUTIVE_IDENTICAL_TOOL_CALL_BATCHES;
	}

	observeResults(messages: ToolResultMessage[]): void {
		const signature = canonicalToolArguments(
			messages.map((message) => ({
				toolName: message.toolName,
				content: message.content,
				isError: message.isError,
			})),
		);
		if (this.lastResultSignature !== undefined && signature !== this.lastResultSignature) {
			this.lastSignature = undefined;
			this.consecutiveBatches = 0;
		}
		this.lastResultSignature = signature;
	}

	reset(): void {
		this.lastSignature = undefined;
		this.lastResultSignature = undefined;
		this.consecutiveBatches = 0;
	}
}

function hasReasoningExhaustedWarning(message: AssistantMessage): boolean {
	return (
		message.diagnostics?.some(
			(diagnostic) => diagnostic.type === "provider_warning" && diagnostic.error?.code === "reasoning_exhausted",
		) ?? false
	);
}

function finalizeToolCallRepetitionMessage(message: AssistantMessage): AssistantMessage {
	const errorMessage = `Repetition loop detected: the model repeated the same tool call batch ${MAX_CONSECUTIVE_IDENTICAL_TOOL_CALL_BATCHES} times; the repeated tools were not executed`;
	return {
		...message,
		content: cloneAssistantContent(message.content),
		usage: cloneUsage(message.usage),
		stopReason: "error",
		errorMessage,
		diagnostics: [
			...(message.diagnostics ?? []),
			createAssistantMessageDiagnostic("agent_repetition_loop", new Error(errorMessage), {
				threshold: MAX_CONSECUTIVE_IDENTICAL_TOOL_CALL_BATCHES,
				kind: "tool_call_batch",
			}),
		],
	};
}

// A provider that stalls on every attempt would retry forever, so cap the
// retries and surface the failure as an error on the final response.
const MAX_STREAM_STALL_RETRIES = 2;
const stallLog = getLogger("agent.stream-stall");
const EMPTY_USAGE: AssistantMessage["usage"] = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

function createAbortError(): Error {
	return new Error(ABORT_ERROR_MESSAGE);
}

function throwIfAborted(signal: AbortSignal | undefined): void {
	if (signal?.aborted) {
		throw createAbortError();
	}
}

function raceWithAbort<T>(operation: Promise<T>, signal: AbortSignal | undefined, onAbort?: () => void): Promise<T> {
	if (!signal) {
		return operation;
	}
	if (signal.aborted) {
		onAbort?.();
		void operation.catch(() => undefined);
		return Promise.reject(createAbortError());
	}

	return new Promise<T>((resolve, reject) => {
		let settled = false;
		const cleanup = () => {
			signal.removeEventListener("abort", abort);
		};
		const abort = () => {
			if (settled) {
				return;
			}
			settled = true;
			cleanup();
			onAbort?.();
			reject(createAbortError());
		};
		signal.addEventListener("abort", abort, { once: true });
		operation.then(
			(value) => {
				if (settled) {
					return;
				}
				settled = true;
				cleanup();
				resolve(value);
			},
			(error: unknown) => {
				if (settled) {
					return;
				}
				settled = true;
				cleanup();
				reject(error);
			},
		);
	});
}

function maybePromiseWithAbort<T>(
	operation: T | Promise<T>,
	signal: AbortSignal | undefined,
	onAbort?: () => void,
): Promise<T> {
	return raceWithAbort(Promise.resolve(operation), signal, onAbort);
}

class RepetitionLoopError extends Error {
	constructor() {
		super("Repetition loop detected");
		this.name = "RepetitionLoopError";
	}
}

class StreamStallError extends Error {
	constructor(readonly timeoutMs: number) {
		super(`No model output for ${Math.round(timeoutMs / 1000)}s`);
		this.name = "StreamStallError";
	}
}

/** Rejects with StreamStallError when the operation produces nothing within timeoutMs. */
function raceWithStallTimeout<T>(operation: Promise<T>, timeoutMs: number): Promise<T> {
	return new Promise<T>((resolve, reject) => {
		const timer = setTimeout(() => reject(new StreamStallError(timeoutMs)), timeoutMs);
		timer.unref?.();
		operation.then(
			(value) => {
				clearTimeout(timer);
				resolve(value);
			},
			(error: unknown) => {
				clearTimeout(timer);
				reject(error);
			},
		);
	});
}

function isAbortError(error: unknown): boolean {
	return error instanceof Error && (error.message === ABORT_ERROR_MESSAGE || error.name === "AbortError");
}

type PostTurnResult<T> = { status: "completed"; value: T } | { status: "aborted" };

async function settlePostTurn<T>(operation: Promise<T>, signal: AbortSignal | undefined): Promise<PostTurnResult<T>> {
	try {
		return { status: "completed", value: await operation };
	} catch (error) {
		if (signal?.aborted && isAbortError(error)) {
			return { status: "aborted" };
		}
		throw error;
	}
}

function cloneAssistantContent(content: AssistantMessage["content"]): AssistantMessage["content"] {
	return content.map((part) => {
		if (part.type === "toolCall") {
			return { ...part, arguments: { ...part.arguments } };
		}
		return { ...part };
	});
}

function cloneUsage(usage: AssistantMessage["usage"]): AssistantMessage["usage"] {
	return { ...usage, cost: { ...usage.cost } };
}

function createAbortedAssistantMessage(
	config: AgentLoopConfig,
	partialMessage: AssistantMessage | null,
): AssistantMessage {
	return {
		role: "assistant",
		content: partialMessage ? cloneAssistantContent(partialMessage.content) : [{ type: "text", text: "" }],
		api: partialMessage?.api ?? config.model.api,
		provider: partialMessage?.provider ?? config.model.provider,
		model: partialMessage?.model ?? config.model.id,
		usage: cloneUsage(partialMessage?.usage ?? EMPTY_USAGE),
		stopReason: "aborted",
		errorMessage: ABORT_ERROR_MESSAGE,
		timestamp: Date.now(),
	};
}

function getTerminalMessage(event: Extract<AssistantMessageEvent, { type: "done" | "error" }>): AssistantMessage {
	return event.type === "done" ? event.message : event.error;
}

function endAgentStreamOnError(
	stream: EventStream<AgentEvent, AgentMessage[]>,
	promise: Promise<AgentMessage[]>,
): void {
	void promise.then(
		(messages) => {
			stream.end(messages);
		},
		() => {
			stream.end([]);
		},
	);
}

async function pollMessagesUnlessAborted(
	poll: (() => AgentMessage[] | Promise<AgentMessage[]>) | undefined,
	signal: AbortSignal | undefined,
): Promise<AgentMessage[]> {
	if (!poll || signal?.aborted) {
		return [];
	}
	return (await maybePromiseWithAbort(poll(), signal)) || [];
}

/**
 * Start an agent loop with a new prompt message.
 * The prompt is added to the context and events are emitted for it.
 */
export function agentLoop(
	prompts: AgentMessage[],
	context: AgentContext,
	config: AgentLoopConfig,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): EventStream<AgentEvent, AgentMessage[]> {
	const stream = createAgentStream();

	endAgentStreamOnError(
		stream,
		runAgentLoop(
			prompts,
			context,
			config,
			async (event) => {
				stream.push(event);
			},
			signal,
			streamFn,
		),
	);

	return stream;
}

/**
 * Continue an agent loop from the current context without adding a new message.
 * Used for retries - context already has user message or tool results.
 *
 * **Important:** The last message in context must convert to a `user` or `toolResult` message
 * via `convertToLlm`. If it doesn't, the LLM provider will reject the request.
 * This cannot be validated here since `convertToLlm` is only called once per turn.
 */
export function agentLoopContinue(
	context: AgentContext,
	config: AgentLoopConfig,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): EventStream<AgentEvent, AgentMessage[]> {
	if (context.messages.length === 0) {
		throw new Error("Cannot continue: no messages in context");
	}

	if (context.messages[context.messages.length - 1].role === "assistant") {
		throw new Error("Cannot continue from message role: assistant");
	}

	const stream = createAgentStream();

	endAgentStreamOnError(
		stream,
		runAgentLoopContinue(
			context,
			config,
			async (event) => {
				stream.push(event);
			},
			signal,
			streamFn,
		),
	);

	return stream;
}

export async function runAgentLoop(
	prompts: AgentMessage[],
	context: AgentContext,
	config: AgentLoopConfig,
	emit: AgentEventSink,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): Promise<AgentMessage[]> {
	const newMessages: AgentMessage[] = [...prompts];
	const currentContext: AgentContext = {
		...context,
		messages: [...context.messages, ...prompts],
	};

	await emit({ type: "agent_start" });
	await emit({ type: "turn_start" });
	for (const prompt of prompts) {
		await emit({ type: "message_start", message: prompt });
		await emit({ type: "message_end", message: prompt });
	}

	await runLoop(currentContext, newMessages, config, signal, emit, streamFn);
	return newMessages;
}

export async function runAgentLoopContinue(
	context: AgentContext,
	config: AgentLoopConfig,
	emit: AgentEventSink,
	signal?: AbortSignal,
	streamFn?: StreamFn,
): Promise<AgentMessage[]> {
	if (context.messages.length === 0) {
		throw new Error("Cannot continue: no messages in context");
	}

	if (context.messages[context.messages.length - 1].role === "assistant") {
		throw new Error("Cannot continue from message role: assistant");
	}

	const newMessages: AgentMessage[] = [];
	const currentContext: AgentContext = { ...context };

	await emit({ type: "agent_start" });
	await emit({ type: "turn_start" });

	await runLoop(currentContext, newMessages, config, signal, emit, streamFn);
	return newMessages;
}

function createAgentStream(): EventStream<AgentEvent, AgentMessage[]> {
	return new EventStream<AgentEvent, AgentMessage[]>(
		(event: AgentEvent) => event.type === "agent_end",
		(event: AgentEvent) => (event.type === "agent_end" ? event.messages : []),
	);
}

async function runLoop(
	currentContext: AgentContext,
	newMessages: AgentMessage[],
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	streamFn?: StreamFn,
): Promise<void> {
	let firstTurn = true;
	let consecutiveIncompleteResponses = 0;
	const toolCallRepetitionDetector = new ToolCallRepetitionDetector();
	let lastTurn: Parameters<NonNullable<AgentLoopConfig["getContinuationMessages"]>>[0] | undefined;
	let pendingMessages: AgentMessage[] = await pollMessagesUnlessAborted(config.getSteeringMessages, signal);

	const shouldStopBeforeTurn = (): boolean => !firstTurn && (config.shouldStopBeforeTurn?.() ?? false);

	while (true) {
		throwIfAborted(signal);
		let hasMoreToolCalls = true;

		while (hasMoreToolCalls || pendingMessages.length > 0) {
			throwIfAborted(signal);
			if (!firstTurn) {
				await emit({ type: "turn_start" });
			} else {
				firstTurn = false;
			}

			if (pendingMessages.length > 0) {
				toolCallRepetitionDetector.reset();
				for (const message of pendingMessages) {
					await emit({ type: "message_start", message });
					await emit({ type: "message_end", message });
					currentContext.messages.push(message);
					newMessages.push(message);
				}
				pendingMessages = [];
			}

			const message = await streamAssistantResponse(currentContext, config, signal, emit, streamFn, (candidate) =>
				toolCallRepetitionDetector.observe(candidate) ? finalizeToolCallRepetitionMessage(candidate) : candidate,
			);
			newMessages.push(message);

			if (message.stopReason === "error" || message.stopReason === "aborted") {
				await emit({ type: "turn_end", message, toolResults: [] });
				await emit({ type: "agent_end", messages: newMessages });
				return;
			}

			const toolCalls = message.content.filter((c) => c.type === "toolCall");
			const hasFinalText = message.content.some((c) => c.type === "text" && c.text.trim().length > 0);
			if (
				message.stopReason === "length" &&
				!hasFinalText &&
				toolCalls.length === 0 &&
				hasReasoningExhaustedWarning(message)
			) {
				message.stopReason = "error";
				message.errorMessage =
					"Provider exhausted the output budget on reasoning without producing an answer; increase the output budget or lower the reasoning effort";
				await emit({ type: "turn_end", message, toolResults: [] });
				await emit({ type: "agent_end", messages: newMessages });
				return;
			}
			const incompleteResponse =
				message.stopReason === "unknown" ||
				((message.stopReason === "stop" || message.stopReason === "length") &&
					!hasFinalText &&
					toolCalls.length === 0);
			if (incompleteResponse) {
				// Continue the same turn so the model can finish, just as it does
				// after tool use. Past the cap, stop without another provider call
				// and surface the failure as an error on the final response.
				consecutiveIncompleteResponses += 1;
				if (consecutiveIncompleteResponses >= MAX_CONSECUTIVE_INCOMPLETE_RESPONSES) {
					message.stopReason = "error";
					message.errorMessage = `Provider closed ${MAX_CONSECUTIVE_INCOMPLETE_RESPONSES} consecutive streams without final content`;
					await emit({ type: "turn_end", message, toolResults: [] });
					await emit({ type: "agent_end", messages: newMessages });
					return;
				}
				hasMoreToolCalls = true;
			} else {
				consecutiveIncompleteResponses = 0;
				hasMoreToolCalls = false;
			}

			const toolResults: ToolResultMessage[] = [];
			if (toolCalls.length > 0) {
				const executedToolBatch = await executeToolCalls(currentContext, message, config, signal, emit);
				toolResults.push(...executedToolBatch.messages);
				toolCallRepetitionDetector.observeResults(toolResults);
				hasMoreToolCalls = !executedToolBatch.terminate;

				for (const result of toolResults) {
					currentContext.messages.push(result);
					newMessages.push(result);
				}
			}

			await emit({ type: "turn_end", message, toolResults });
			if (signal?.aborted) {
				await emit({ type: "agent_end", messages: newMessages });
				return;
			}
			lastTurn = {
				message,
				toolResults,
				context: currentContext,
				newMessages,
			};

			const shouldStopResult = await settlePostTurn(
				maybePromiseWithAbort(
					config.shouldStopAfterTurn?.({
						message,
						toolResults,
						context: currentContext,
						newMessages,
					}) ?? false,
					signal,
				),
				signal,
			);
			if (shouldStopResult.status === "aborted") {
				await emit({ type: "agent_end", messages: newMessages });
				return;
			}
			if (shouldStopResult.value || shouldStopBeforeTurn()) {
				await emit({ type: "agent_end", messages: newMessages });
				return;
			}

			const steeringMessagesResult = await settlePostTurn(
				pollMessagesUnlessAborted(config.getSteeringMessages, signal),
				signal,
			);
			if (steeringMessagesResult.status === "aborted") {
				await emit({ type: "agent_end", messages: newMessages });
				return;
			}
			pendingMessages = steeringMessagesResult.value;
			// Steering drained by this poll owns the turn boundary; stop only when it was empty.
			if (pendingMessages.length === 0 && shouldStopBeforeTurn()) {
				await emit({ type: "agent_end", messages: newMessages });
				return;
			}
		}

		if (shouldStopBeforeTurn()) break;
		const followUpMessagesResult = await settlePostTurn(
			pollMessagesUnlessAborted(config.getFollowUpMessages, signal),
			signal,
		);
		if (followUpMessagesResult.status === "aborted") {
			await emit({ type: "agent_end", messages: newMessages });
			return;
		}
		const followUpMessages = followUpMessagesResult.value;
		if (followUpMessages.length > 0) {
			pendingMessages = followUpMessages;
			continue;
		}

		if (shouldStopBeforeTurn()) break;
		const continuationMessagesResult = lastTurn
			? await settlePostTurn(
					maybePromiseWithAbort(config.getContinuationMessages?.(lastTurn, signal) ?? [], signal),
					signal,
				)
			: ({ status: "completed", value: [] } satisfies PostTurnResult<AgentMessage[]>);
		if (continuationMessagesResult.status === "aborted") {
			await emit({ type: "agent_end", messages: newMessages });
			return;
		}
		const continuationMessages = continuationMessagesResult.value || [];
		if (continuationMessages.length > 0) {
			pendingMessages = continuationMessages;
			continue;
		}

		break;
	}

	await emit({ type: "agent_end", messages: newMessages });
}

async function streamAssistantResponse(
	context: AgentContext,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	streamFn?: StreamFn,
	finalizeMessage: (message: AssistantMessage) => AssistantMessage = (message) => message,
): Promise<AssistantMessage> {
	const stallTimeoutMs = config.streamStallTimeoutMs ?? STREAM_STALL_TIMEOUT_MS;
	let partialMessage: AssistantMessage | null = null;
	let addedPartial = false;
	for (let attempt = 0; ; attempt++) {
		try {
			return await streamAssistantAttempt(
				context,
				config,
				signal,
				emit,
				streamFn,
				stallTimeoutMs,
				(partial, added) => {
					partialMessage = partial;
					addedPartial = added;
				},
				finalizeMessage,
			);
		} catch (error) {
			if (error instanceof RepetitionLoopError) {
				const threshold = repetitionLoopThreshold(config);
				const finalMessage = finalizeRepetitionLoopMessage(config, addedPartial ? partialMessage : null, threshold);
				if (addedPartial && context.messages.at(-1) === partialMessage) {
					context.messages[context.messages.length - 1] = finalMessage;
				} else {
					context.messages.push(finalMessage);
					await emit({ type: "message_start", message: { ...finalMessage } });
				}
				await emit({ type: "message_end", message: finalMessage });
				return finalMessage;
			}
			if (!(error instanceof StreamStallError) || signal?.aborted) {
				throw error;
			}
			stallLog.warn("provider stream stalled", {
				provider: config.model.provider,
				model: config.model.id,
				attempt: attempt + 1,
				maxAttempts: MAX_STREAM_STALL_RETRIES + 1,
				timeoutMs: stallTimeoutMs,
			});
			if (attempt < MAX_STREAM_STALL_RETRIES) {
				// Roll back the abandoned partial so the retry streams into a clean context.
				if (addedPartial && context.messages.at(-1) === partialMessage) {
					context.messages.pop();
				}
				continue;
			}
			const finalMessage = createAbortedAssistantMessage(config, addedPartial ? partialMessage : null);
			finalMessage.stopReason = "error";
			finalMessage.errorMessage = `${error.message}; gave up after ${attempt + 1} attempts`;
			if (addedPartial && context.messages.at(-1) === partialMessage) {
				context.messages[context.messages.length - 1] = finalMessage;
			} else {
				context.messages.push(finalMessage);
				await emit({ type: "message_start", message: { ...finalMessage } });
			}
			await emit({ type: "message_end", message: finalMessage });
			return finalMessage;
		}
	}
}

async function streamAssistantAttempt(
	context: AgentContext,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	streamFn: StreamFn | undefined,
	stallTimeoutMs: number,
	trackPartial: (partial: AssistantMessage | null, added: boolean) => void,
	finalizeMessage: (message: AssistantMessage) => AssistantMessage,
): Promise<AssistantMessage> {
	// The provider request listens on a linked signal so a stall can abort the
	// underlying HTTP connection while user cancellation keeps using `signal`.
	const requestController = new AbortController();
	const forwardAbort = () => requestController.abort();
	if (signal?.aborted) {
		requestController.abort();
	} else {
		signal?.addEventListener("abort", forwardAbort, { once: true });
	}
	try {
		throwIfAborted(signal);
		let messages = context.messages;
		if (config.transformContext) {
			messages = await maybePromiseWithAbort(config.transformContext(messages, signal), signal);
		}

		const llmMessages = await maybePromiseWithAbort(config.convertToLlm(messages), signal);

		const streamFunction = streamFn || streamSimple;

		const resolvedApiKey =
			(config.getApiKey
				? await maybePromiseWithAbort(config.getApiKey(config.model.provider), signal)
				: undefined) || config.apiKey;

		const llmContext: Context = {
			systemPrompt: config.getSystemPrompt?.() ?? context.systemPrompt,
			messages: llmMessages,
			tools: context.tools,
		};

		try {
			const response = await maybePromiseWithAbort(
				raceWithStallTimeout(
					Promise.resolve(
						streamFunction(config.model, llmContext, {
							...config,
							apiKey: resolvedApiKey,
							signal: requestController.signal,
						}),
					),
					stallTimeoutMs,
				),
				signal,
			);
			return await consumeAssistantStream(
				response,
				context,
				config,
				signal,
				emit,
				stallTimeoutMs,
				trackPartial,
				requestController,
				finalizeMessage,
			);
		} catch (error) {
			if (error instanceof StreamStallError) {
				requestController.abort();
			}
			throw error;
		}
	} catch (error) {
		if (signal?.aborted && isAbortError(error)) {
			const finalMessage = createAbortedAssistantMessage(config, null);
			context.messages.push(finalMessage);
			await emit({ type: "message_start", message: { ...finalMessage } });
			await emit({ type: "message_end", message: finalMessage });
			return finalMessage;
		}
		throw error;
	} finally {
		signal?.removeEventListener("abort", forwardAbort);
	}
}

/**
 * Consumes one provider stream to completion. Rejects with StreamStallError when
 * no stream events arrive within stallTimeoutMs; the caller decides whether to
 * retry or surface the failure.
 */
async function consumeAssistantStream(
	response: Awaited<ReturnType<StreamFn>>,
	context: AgentContext,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
	stallTimeoutMs: number,
	trackPartial: (partial: AssistantMessage | null, added: boolean) => void,
	requestController: AbortController,
	finalizeMessage: (message: AssistantMessage) => AssistantMessage,
): Promise<AssistantMessage> {
	let partialMessage: AssistantMessage | null = null;
	let addedPartial = false;
	const repetitionThreshold = repetitionLoopThreshold(config);
	const repetitionDetector = new RepetitionDetector(repetitionThreshold);
	const finishAbortedMessage = async () => {
		const finalMessage = createAbortedAssistantMessage(config, partialMessage);
		if (addedPartial) {
			context.messages[context.messages.length - 1] = finalMessage;
		} else {
			context.messages.push(finalMessage);
			await emit({ type: "message_start", message: { ...finalMessage } });
		}
		await emit({ type: "message_end", message: finalMessage });
		return finalMessage;
	};

	try {
		const iterator = response[Symbol.asyncIterator]();
		const closeIterator = () => {
			void Promise.resolve(iterator.return?.()).catch(() => undefined);
		};
		while (true) {
			const next = await raceWithAbort<IteratorResult<AssistantMessageEvent>>(
				raceWithStallTimeout(iterator.next(), stallTimeoutMs),
				signal,
				closeIterator,
			);
			if (next.done) {
				break;
			}
			const event = next.value;
			switch (event.type) {
				case "start":
					partialMessage = event.partial;
					context.messages.push(partialMessage);
					addedPartial = true;
					trackPartial(partialMessage, addedPartial);
					await emit({ type: "message_start", message: { ...partialMessage } });
					break;

				case "text_start":
				case "text_delta":
				case "text_end":
				case "thinking_start":
				case "thinking_delta":
				case "thinking_end":
				case "toolcall_start":
				case "toolcall_delta":
				case "toolcall_end":
					if ((event.type === "text_delta" || event.type === "thinking_delta") && repetitionThreshold > 0) {
						if (repetitionDetector.observeText(event.delta)) {
							// Cancel the provider request first so the HTTP connection stops
							// consuming output, then unwind with the dedicated error.
							requestController.abort();
							throw new RepetitionLoopError();
						}
					}
					if (partialMessage) {
						partialMessage = event.partial;
						context.messages[context.messages.length - 1] = partialMessage;
						await emit({
							type: "message_update",
							assistantMessageEvent: event,
							message: { ...partialMessage },
						});
					}
					break;

				case "done":
				case "error": {
					let finalMessage = getTerminalMessage(event);
					try {
						finalMessage = await maybePromiseWithAbort(
							raceWithStallTimeout(response.result(), stallTimeoutMs),
							signal,
						);
					} catch (error) {
						if (!signal?.aborted || !isAbortError(error)) {
							throw error;
						}
					}
					finalMessage = finalizeMessage(finalMessage);
					if (addedPartial) {
						context.messages[context.messages.length - 1] = finalMessage;
					} else {
						context.messages.push(finalMessage);
					}
					if (!addedPartial) {
						await emit({ type: "message_start", message: { ...finalMessage } });
					}
					await emit({ type: "message_end", message: finalMessage });
					return finalMessage;
				}
			}
		}

		const finalMessage = finalizeMessage(
			await maybePromiseWithAbort(raceWithStallTimeout(response.result(), stallTimeoutMs), signal),
		);
		if (addedPartial) {
			context.messages[context.messages.length - 1] = finalMessage;
		} else {
			context.messages.push(finalMessage);
			await emit({ type: "message_start", message: { ...finalMessage } });
		}
		await emit({ type: "message_end", message: finalMessage });
		return finalMessage;
	} catch (error) {
		if (signal?.aborted && isAbortError(error)) {
			return finishAbortedMessage();
		}
		throw error;
	}
}

async function executeToolCalls(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<ExecutedToolCallBatch> {
	const toolCalls = assistantMessage.content.filter((c) => c.type === "toolCall");
	const hasSequentialToolCall = toolCalls.some(
		(tc) => currentContext.tools?.find((t) => t.name === tc.name)?.executionMode === "sequential",
	);
	if (config.toolExecution === "sequential" || hasSequentialToolCall) {
		return executeToolCallsSequential(currentContext, assistantMessage, toolCalls, config, signal, emit);
	}
	return executeToolCallsParallel(currentContext, assistantMessage, toolCalls, config, signal, emit);
}

type ExecutedToolCallBatch = {
	messages: ToolResultMessage[];
	terminate: boolean;
};

async function executeToolCallsSequential(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	toolCalls: AgentToolCall[],
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<ExecutedToolCallBatch> {
	const finalizedCalls: FinalizedToolCallOutcome[] = [];
	const messages: ToolResultMessage[] = [];

	for (const toolCall of toolCalls) {
		if (signal?.aborted) {
			break;
		}

		await emit({
			type: "tool_execution_start",
			toolCallId: toolCall.id,
			toolName: toolCall.name,
			args: toolCall.arguments,
		});

		const preparation = await prepareToolCall(currentContext, assistantMessage, toolCall, config, signal);
		let finalized: FinalizedToolCallOutcome;
		if (preparation.kind === "immediate") {
			finalized = {
				toolCall,
				result: preparation.result,
				isError: preparation.isError,
			};
		} else {
			const executed = await executePreparedToolCall(preparation, signal, emit);
			finalized = await finalizeExecutedToolCall(
				currentContext,
				assistantMessage,
				preparation,
				executed,
				config,
				signal,
			);
		}

		await emitToolExecutionEnd(finalized, emit);
		const toolResultMessage = createToolResultMessage(finalized);
		await emitToolResultMessage(toolResultMessage, emit);
		finalizedCalls.push(finalized);
		messages.push(toolResultMessage);

		if (signal?.aborted) {
			break;
		}
	}

	return {
		messages,
		terminate: shouldTerminateToolBatch(finalizedCalls),
	};
}

async function executeToolCallsParallel(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	toolCalls: AgentToolCall[],
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<ExecutedToolCallBatch> {
	const finalizedCalls: FinalizedToolCallEntry[] = [];

	for (const toolCall of toolCalls) {
		await emit({
			type: "tool_execution_start",
			toolCallId: toolCall.id,
			toolName: toolCall.name,
			args: toolCall.arguments,
		});

		const preparation = await prepareToolCall(currentContext, assistantMessage, toolCall, config, signal);
		if (preparation.kind === "immediate") {
			const finalized = {
				toolCall,
				result: preparation.result,
				isError: preparation.isError,
			} satisfies FinalizedToolCallOutcome;
			await emitToolExecutionEnd(finalized, emit);
			finalizedCalls.push(finalized);
			continue;
		}

		finalizedCalls.push(async () => {
			const executed = await executePreparedToolCall(preparation, signal, emit);
			const finalized = await finalizeExecutedToolCall(
				currentContext,
				assistantMessage,
				preparation,
				executed,
				config,
				signal,
			);
			await emitToolExecutionEnd(finalized, emit);
			return finalized;
		});
	}

	const orderedFinalizedCalls = await Promise.all(
		finalizedCalls.map((entry) => (typeof entry === "function" ? entry() : Promise.resolve(entry))),
	);
	const messages: ToolResultMessage[] = [];
	for (const finalized of orderedFinalizedCalls) {
		const toolResultMessage = createToolResultMessage(finalized);
		await emitToolResultMessage(toolResultMessage, emit);
		messages.push(toolResultMessage);
	}

	return {
		messages,
		terminate: shouldTerminateToolBatch(orderedFinalizedCalls),
	};
}

type PreparedToolCall = {
	kind: "prepared";
	toolCall: AgentToolCall;
	tool: AgentTool<any>;
	args: unknown;
};

type ImmediateToolCallOutcome = {
	kind: "immediate";
	result: AgentToolResult<any>;
	isError: boolean;
};

type ExecutedToolCallOutcome = {
	result: AgentToolResult<any>;
	isError: boolean;
};

type FinalizedToolCallOutcome = {
	toolCall: AgentToolCall;
	result: AgentToolResult<any>;
	isError: boolean;
};

type FinalizedToolCallEntry = FinalizedToolCallOutcome | (() => Promise<FinalizedToolCallOutcome>);

function shouldTerminateToolBatch(finalizedCalls: FinalizedToolCallOutcome[]): boolean {
	return finalizedCalls.length > 0 && finalizedCalls.every((finalized) => finalized.result.terminate === true);
}

function prepareToolCallArguments(tool: AgentTool<any>, toolCall: AgentToolCall): AgentToolCall {
	if (!tool.prepareArguments) {
		return toolCall;
	}
	const preparedArguments = tool.prepareArguments(toolCall.arguments);
	if (preparedArguments === toolCall.arguments) {
		return toolCall;
	}
	return {
		...toolCall,
		arguments: preparedArguments as Record<string, any>,
	};
}

async function prepareToolCall(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	toolCall: AgentToolCall,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
): Promise<PreparedToolCall | ImmediateToolCallOutcome> {
	const tool = currentContext.tools?.find((t) => t.name === toolCall.name);
	if (!tool) {
		return {
			kind: "immediate",
			result: createErrorToolResult(`Tool ${toolCall.name} not found`),
			isError: true,
		};
	}

	try {
		const preparedToolCall = prepareToolCallArguments(tool, toolCall);
		const validatedArgs = validateToolArguments(tool, preparedToolCall);
		if (config.beforeToolCall) {
			const beforeResult = await maybePromiseWithAbort(
				config.beforeToolCall(
					{
						assistantMessage,
						toolCall,
						args: validatedArgs,
						context: currentContext,
					},
					signal,
				),
				signal,
			);
			if (beforeResult?.block) {
				return {
					kind: "immediate",
					result: createErrorToolResult(beforeResult.reason || "Tool execution was blocked"),
					isError: true,
				};
			}
		}
		return {
			kind: "prepared",
			toolCall,
			tool,
			args: validatedArgs,
		};
	} catch (error) {
		return {
			kind: "immediate",
			result: createErrorToolResult(error instanceof Error ? error.message : String(error)),
			isError: true,
		};
	}
}

async function executePreparedToolCall(
	prepared: PreparedToolCall,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<ExecutedToolCallOutcome> {
	const updateEvents: Promise<void>[] = [];
	let acceptingUpdates = true;

	try {
		throwIfAborted(signal);
		const result = await raceWithAbort(
			prepared.tool.execute(prepared.toolCall.id, prepared.args as never, signal, (partialResult) => {
				if (!acceptingUpdates || signal?.aborted) {
					return;
				}
				updateEvents.push(
					Promise.resolve(
						emit({
							type: "tool_execution_update",
							toolCallId: prepared.toolCall.id,
							toolName: prepared.toolCall.name,
							args: prepared.toolCall.arguments,
							partialResult,
						}),
					),
				);
			}),
			signal,
		);
		acceptingUpdates = false;
		try {
			await raceWithAbort(
				Promise.all(updateEvents).then(() => undefined),
				signal,
			);
		} catch (error) {
			if (!signal?.aborted || !isAbortError(error)) {
				throw error;
			}
		}
		return { result, isError: false };
	} catch (error) {
		acceptingUpdates = false;
		await raceWithAbort(
			Promise.all(updateEvents).then(() => undefined),
			signal,
		).catch(() => undefined);
		return {
			result: createErrorToolResult(
				signal?.aborted ? "Tool execution aborted" : error instanceof Error ? error.message : String(error),
			),
			isError: true,
		};
	}
}

async function finalizeExecutedToolCall(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	prepared: PreparedToolCall,
	executed: ExecutedToolCallOutcome,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
): Promise<FinalizedToolCallOutcome> {
	let result = executed.result;
	let isError = executed.isError;

	if (config.afterToolCall) {
		try {
			const afterResult = await maybePromiseWithAbort(
				config.afterToolCall(
					{
						assistantMessage,
						toolCall: prepared.toolCall,
						args: prepared.args,
						result,
						isError,
						context: currentContext,
					},
					signal,
				),
				signal,
			);
			if (afterResult) {
				result = {
					content: afterResult.content ?? result.content,
					details: afterResult.details ?? result.details,
					terminate: afterResult.terminate ?? result.terminate,
				};
				isError = afterResult.isError ?? isError;
			}
		} catch (error) {
			result = createErrorToolResult(error instanceof Error ? error.message : String(error));
			isError = true;
		}
	}

	return {
		toolCall: prepared.toolCall,
		result,
		isError,
	};
}

function createErrorToolResult(message: string): AgentToolResult<any> {
	return {
		content: [{ type: "text", text: message }],
		details: {},
	};
}

async function emitToolExecutionEnd(finalized: FinalizedToolCallOutcome, emit: AgentEventSink): Promise<void> {
	await emit({
		type: "tool_execution_end",
		toolCallId: finalized.toolCall.id,
		toolName: finalized.toolCall.name,
		result: finalized.result,
		isError: finalized.isError,
	});
}

function createToolResultMessage(finalized: FinalizedToolCallOutcome): ToolResultMessage {
	return {
		role: "toolResult",
		toolCallId: finalized.toolCall.id,
		toolName: finalized.toolCall.name,
		content: finalized.result.content,
		details: finalized.result.details,
		isError: finalized.isError,
		timestamp: Date.now(),
	};
}

async function emitToolResultMessage(toolResultMessage: ToolResultMessage, emit: AgentEventSink): Promise<void> {
	await emit({ type: "message_start", message: toolResultMessage });
	await emit({ type: "message_end", message: toolResultMessage });
}
