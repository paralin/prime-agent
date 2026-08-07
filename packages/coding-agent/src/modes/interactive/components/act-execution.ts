import type { Usage } from "@earendil-works/pi-ai";
import { truncateToWidth, VersionedRenderCache, wrapTextWithAnsi } from "@earendil-works/pi-tui";
import type { ActCancellationCapability } from "../../../core/act-cancellation.js";
import type { ActEventModel, ActProjectionEvent } from "../../../core/act-events.js";
import { theme } from "../theme/theme.js";
import { getWorkingPulseFrame, WORKING_ICON_FRAMES, workingIconFrame } from "../theme/working-icon.js";
import { keyHint } from "./keybinding-hints.js";

export const ACT_COMPONENT_PROMPT_MAX_CHARS = 16_384;
export const ACT_COMPONENT_PROGRESS_MAX_CHARS = 65_536;
export const ACT_COMPONENT_MAX_CELLS = 32;
export const ACT_COMPONENT_MAX_ACTS_PER_TOOL = 32;

export interface ActExecutionCellState {
	id: string;
	scopedId: string;
	code: string;
	codeTruncated: boolean;
	status: "running" | "ok" | "error" | "cancelled";
	stdout: string;
	stdoutTruncated: boolean;
	stderr: string;
	stderrTruncated: boolean;
	result?: string;
	resultTruncated: boolean;
	error?: string;
	errorTruncated: boolean;
}

export interface ActExecutionState {
	actId: string;
	outerToolCallId: string;
	sequence: number;
	prompt: string;
	promptTruncated: boolean;
	model?: ActEventModel;
	cancellationCapability?: ActCancellationCapability;
	status: "running" | "done" | "error" | "cancelled";
	thinking: string;
	thinkingTruncated: boolean;
	text: string;
	textTruncated: boolean;
	cells: ActExecutionCellState[];
	progressChars: number;
	progressTruncated: boolean;
	usage?: Usage;
	error?: string;
	errorTruncated: boolean;
}

interface RetainedText {
	text: string;
	truncated: boolean;
	chars: number;
}

function retainPrompt(text: string, sourceTruncated: boolean): RetainedText {
	const retained = text.slice(0, ACT_COMPONENT_PROMPT_MAX_CHARS);
	return {
		text: retained,
		truncated: sourceTruncated || retained.length < text.length,
		chars: retained.length,
	};
}

function retainProgress(state: ActExecutionState, text: string, sourceTruncated: boolean): RetainedText {
	const remaining = Math.max(0, ACT_COMPONENT_PROGRESS_MAX_CHARS - state.progressChars);
	const retained = text.slice(0, remaining);
	return {
		text: retained,
		truncated: sourceTruncated || retained.length < text.length,
		chars: retained.length,
	};
}

function appendStream(
	state: ActExecutionState,
	field: "thinking" | "text",
	truncatedField: "thinkingTruncated" | "textTruncated",
	text: string,
	sourceTruncated: boolean,
): ActExecutionState {
	const retained = retainProgress(state, text, sourceTruncated);
	return {
		...state,
		[field]: state[field] + retained.text,
		[truncatedField]: state[truncatedField] || retained.truncated,
		progressChars: state.progressChars + retained.chars,
		progressTruncated: state.progressTruncated || retained.truncated,
	};
}

function emptyCell(actId: string, cellId: string): ActExecutionCellState {
	return {
		id: cellId,
		scopedId: `${actId}:${cellId}`,
		code: "",
		codeTruncated: false,
		status: "running",
		stdout: "",
		stdoutTruncated: false,
		stderr: "",
		stderrTruncated: false,
		resultTruncated: false,
		errorTruncated: false,
	};
}

function replaceCell(state: ActExecutionState, cell: ActExecutionCellState): ActExecutionCellState[] {
	const index = state.cells.findIndex((candidate) => candidate.scopedId === cell.scopedId);
	if (index < 0) return [...state.cells, cell];
	const cells = [...state.cells];
	cells[index] = cell;
	return cells;
}

function retainCellFields(
	state: ActExecutionState,
	values: Array<{ text: string | undefined; sourceTruncated: boolean }>,
): { retained: RetainedText[]; chars: number; truncated: boolean } {
	let cursor = state;
	const retained: RetainedText[] = [];
	let chars = 0;
	let truncated = false;
	for (const value of values) {
		const result = retainProgress(cursor, value.text ?? "", value.sourceTruncated);
		retained.push(result);
		chars += result.chars;
		truncated ||= result.truncated;
		cursor = { ...cursor, progressChars: cursor.progressChars + result.chars };
	}
	return { retained, chars, truncated };
}

export function createActExecutionState(event: ActProjectionEvent): ActExecutionState {
	const state: ActExecutionState = {
		actId: event.actId,
		outerToolCallId: event.outerToolCallId,
		sequence: 0,
		prompt: "",
		promptTruncated: false,
		status: "running",
		thinking: "",
		thinkingTruncated: false,
		text: "",
		textTruncated: false,
		cells: [],
		progressChars: 0,
		progressTruncated: false,
		errorTruncated: false,
	};
	return reduceActExecutionState(state, event);
}

export function reduceActExecutionState(state: ActExecutionState, event: ActProjectionEvent): ActExecutionState {
	if (
		state.status !== "running" ||
		event.actId !== state.actId ||
		event.outerToolCallId !== state.outerToolCallId ||
		event.sequence <= state.sequence
	) {
		return state;
	}
	const next = { ...state, sequence: event.sequence };
	switch (event.event) {
		case "start": {
			const prompt = retainPrompt(event.prompt, event.promptTruncated);
			return {
				...next,
				prompt: prompt.text,
				promptTruncated: prompt.truncated,
				model: event.model,
				cancellationCapability: event.cancellationCapability,
				status: "running",
			};
		}
		case "assistant_delta":
			return event.stream === "thinking"
				? appendStream(next, "thinking", "thinkingTruncated", event.text, event.textTruncated)
				: appendStream(next, "text", "textTruncated", event.text, event.textTruncated);
		case "cell_start": {
			let cell = next.cells.find((candidate) => candidate.scopedId === `${event.actId}:${event.cellId}`);
			if (!cell && next.cells.length >= ACT_COMPONENT_MAX_CELLS) {
				return { ...next, progressTruncated: true };
			}
			cell ??= emptyCell(event.actId, event.cellId);
			const code = retainProgress(next, event.code, event.codeTruncated);
			cell = { ...cell, code: code.text, codeTruncated: code.truncated, status: "running" };
			return {
				...next,
				cells: replaceCell(next, cell),
				progressChars: next.progressChars + code.chars,
				progressTruncated: next.progressTruncated || code.truncated,
			};
		}
		case "cell_terminal": {
			let cell = next.cells.find((candidate) => candidate.scopedId === `${event.actId}:${event.cellId}`);
			if (!cell && next.cells.length >= ACT_COMPONENT_MAX_CELLS) {
				return { ...next, progressTruncated: true };
			}
			cell ??= emptyCell(event.actId, event.cellId);
			const fields = retainCellFields(next, [
				{ text: event.stdout, sourceTruncated: event.stdoutTruncated },
				{ text: event.stderr, sourceTruncated: event.stderrTruncated },
				{ text: event.result, sourceTruncated: event.resultTruncated },
				{ text: event.error, sourceTruncated: event.errorTruncated },
			]);
			const [stdout, stderr, result, error] = fields.retained;
			cell = {
				...cell,
				status: event.status,
				stdout: stdout?.text ?? "",
				stdoutTruncated: stdout?.truncated ?? false,
				stderr: stderr?.text ?? "",
				stderrTruncated: stderr?.truncated ?? false,
				...(event.result !== undefined ? { result: result?.text ?? "" } : {}),
				resultTruncated: result?.truncated ?? false,
				...(event.error !== undefined ? { error: error?.text ?? "" } : {}),
				errorTruncated: error?.truncated ?? false,
			};
			return {
				...next,
				cells: replaceCell(next, cell),
				progressChars: next.progressChars + fields.chars,
				progressTruncated: next.progressTruncated || fields.truncated,
			};
		}
		case "terminal": {
			const prompt = retainPrompt(event.prompt, event.promptTruncated);
			return {
				...next,
				prompt: prompt.text,
				promptTruncated: prompt.truncated,
				model: event.model,
				cancellationCapability: event.cancellationCapability,
				status: event.status,
				usage: event.usage,
				...(event.error !== undefined ? { error: event.error } : {}),
				errorTruncated: event.errorTruncated,
			};
		}
	}
}

function cancellationLabel(capability: ActCancellationCapability | undefined): string {
	if (capability === "posix-managed") return "POSIX managed cancellation";
	if (capability === "cooperative-only") return "cooperative cancellation only";
	return "cancellation capability pending";
}

function modelLabel(model: ActEventModel | undefined): string {
	return model ? `${model.provider}/${model.id}` : "resolving model";
}

function usageLabel(usage: Usage | undefined): string | undefined {
	if (!usage) return undefined;
	const parts: string[] = [];
	if (typeof usage.totalTokens === "number") parts.push(`${usage.totalTokens.toLocaleString()} tokens`);
	if (typeof usage.cost?.total === "number") parts.push(`$${usage.cost.total.toFixed(4)}`);
	return parts.join(" · ") || undefined;
}

function boundedSuffix(truncated: boolean): string {
	return truncated ? " [truncated]" : "";
}

export class ActExecutionComponent {
	private readonly renderCache = new VersionedRenderCache();
	private state: ActExecutionState;
	private stateVersion = 0;
	private expanded = false;

	constructor(event: ActProjectionEvent) {
		this.state = createActExecutionState(event);
	}

	get actId(): string {
		return this.state.actId;
	}

	get outerToolCallId(): string {
		return this.state.outerToolCallId;
	}

	get snapshot(): Readonly<ActExecutionState> {
		return this.state;
	}

	update(event: ActProjectionEvent): void {
		const next = reduceActExecutionState(this.state, event);
		if (next === this.state) return;
		this.state = next;
		this.stateVersion += 1;
		this.renderCache.invalidate();
	}

	setExpanded(expanded: boolean): void {
		if (this.expanded === expanded) return;
		this.expanded = expanded;
		this.stateVersion += 1;
		this.renderCache.invalidate();
	}

	invalidate(): void {
		this.renderCache.invalidate();
	}

	render(width: number): string[] {
		const safeWidth = Math.max(1, width);
		const frames = WORKING_ICON_FRAMES.length;
		const cacheVersion =
			this.state.status === "running"
				? this.stateVersion * frames + (getWorkingPulseFrame() % frames)
				: this.stateVersion * frames;
		const cached = this.renderCache.get(safeWidth, cacheVersion);
		if (cached) return cached;
		const lines = [truncateToWidth(`   ${this.summaryLine()}`, safeWidth, "")];
		if (this.expanded) this.renderExpanded(lines, safeWidth);
		return this.renderCache.set(safeWidth, cacheVersion, lines);
	}

	private summaryLine(): string {
		const marker =
			this.state.status === "running"
				? theme.fg("bashMode", workingIconFrame(getWorkingPulseFrame()))
				: this.state.status === "done"
					? theme.fg("success", "✓")
					: theme.fg(this.state.status === "error" ? "error" : "warning", "✗");
		const status = this.state.status === "running" ? "running" : this.state.status;
		const parts = [
			`${marker} ${theme.fg("accent", "Act")}`,
			`Directing model → ${modelLabel(this.state.model)}`,
			status,
			`${this.state.cells.length} cell${this.state.cells.length === 1 ? "" : "s"}`,
		];
		const usage = usageLabel(this.state.usage);
		if (usage) parts.push(usage);
		return parts.join(theme.fg("dim", " · "));
	}

	private renderExpanded(lines: string[], width: number): void {
		this.addText(lines, width, "prompt", this.state.prompt, this.state.promptTruncated);
		this.addText(lines, width, "cancellation", cancellationLabel(this.state.cancellationCapability), false);
		if (this.state.thinking)
			this.addText(lines, width, "thinking", this.state.thinking, this.state.thinkingTruncated);
		if (this.state.text) this.addText(lines, width, "text", this.state.text, this.state.textTruncated);
		for (const cell of this.state.cells) this.renderCell(lines, width, cell);
		if (this.state.progressTruncated) this.addText(lines, width, "progress", "additional Act progress omitted", true);
		if (this.state.error) this.addText(lines, width, this.state.status, this.state.error, this.state.errorTruncated);
		const usage = usageLabel(this.state.usage);
		if (usage) this.addText(lines, width, "usage", usage, false);
		if (this.state.status === "running") {
			// Informational only: InteractiveMode's existing submit/interrupt handlers
			// remain the sole steering and cancellation paths.
			this.addText(
				lines,
				width,
				"controls",
				`type a message to steer · ${keyHint("app.interrupt", "to cancel")}`,
				false,
			);
		}
	}

	private renderCell(lines: string[], width: number, cell: ActExecutionCellState): void {
		this.addText(lines, width, `cell ${cell.id} · ${cell.status}`, cell.code, cell.codeTruncated);
		if (cell.stdout) this.addText(lines, width, "stdout", cell.stdout, cell.stdoutTruncated);
		if (cell.stderr) this.addText(lines, width, "stderr", cell.stderr, cell.stderrTruncated);
		if (cell.result !== undefined) this.addText(lines, width, "result", cell.result, cell.resultTruncated);
		if (cell.error !== undefined) this.addText(lines, width, "error", cell.error, cell.errorTruncated);
	}

	private addText(lines: string[], width: number, label: string, text: string, truncated: boolean): void {
		const prefix = `     ${theme.fg("muted", `${label}: `)}`;
		const content = `${text || "—"}${boundedSuffix(truncated)}`;
		const available = Math.max(1, width - 5);
		const wrapped = wrapTextWithAnsi(content, available);
		for (const [index, line] of (wrapped.length > 0 ? wrapped : [""]).entries()) {
			const linePrefix = index === 0 ? prefix : " ".repeat(5);
			lines.push(truncateToWidth(`${linePrefix}${line}`, width, ""));
		}
	}
}
