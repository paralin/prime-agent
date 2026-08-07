import type { Usage } from "@earendil-works/pi-ai";
import type { TUI } from "@earendil-works/pi-tui";
import stripAnsi from "strip-ansi";
import { beforeAll, describe, expect, it, vi } from "vitest";
import type { ActProjectionEvent } from "../src/core/act-events.js";
import {
	ACT_COMPONENT_MAX_ACTS_PER_TOOL,
	ACT_COMPONENT_PROGRESS_MAX_CHARS,
	ACT_COMPONENT_PROMPT_MAX_CHARS,
	ActExecutionComponent,
} from "../src/modes/interactive/components/act-execution.js";
import { ToolExecutionComponent } from "../src/modes/interactive/components/tool-execution.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";
import { setWorkingPulseFrame } from "../src/modes/interactive/theme/working-icon.js";

const usage: Usage = {
	input: 10,
	output: 5,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 15,
	cost: { input: 0.001, output: 0.002, cacheRead: 0, cacheWrite: 0, total: 0.003 },
};

type ActEventBody<Event> = Event extends ActProjectionEvent ? Omit<Event, "type" | "actId" | "outerToolCallId"> : never;

function event(
	body: ActEventBody<ActProjectionEvent>,
	identity: { actId?: string; outerToolCallId?: string } = {},
): ActProjectionEvent {
	return {
		type: "act_event",
		actId: identity.actId ?? "act-a",
		outerToolCallId: identity.outerToolCallId ?? "outer-a",
		...body,
	} as ActProjectionEvent;
}

function start(model = "luna-model", identity: { actId?: string; outerToolCallId?: string } = {}): ActProjectionEvent {
	return event(
		{
			sequence: 1,
			event: "start",
			prompt: "inspect retained state",
			promptTruncated: false,
			model: { provider: "test", id: model },
			cancellationCapability: "posix-managed",
		},
		identity,
	);
}

function terminal(
	status: "done" | "error" | "cancelled",
	sequence: number,
	options: {
		model?: string;
		capability?: "posix-managed" | "cooperative-only";
		actId?: string;
		outerToolCallId?: string;
		error?: string;
	} = {},
): ActProjectionEvent {
	return event(
		{
			sequence,
			event: "terminal",
			status,
			prompt: "inspect retained state",
			promptTruncated: false,
			model: { provider: "test", id: options.model ?? "luna-model" },
			cancellationCapability: options.capability ?? "posix-managed",
			usage,
			...(options.error ? { error: options.error } : {}),
			errorTruncated: false,
		},
		{ actId: options.actId, outerToolCallId: options.outerToolCallId },
	);
}

function render(component: { render(width: number): string[] }, width = 120): string {
	return stripAnsi(component.render(width).join("\n"));
}

function fakeTui(): TUI {
	return { requestRender: vi.fn() } as unknown as TUI;
}

describe("ActExecutionComponent", () => {
	beforeAll(() => {
		initTheme("dark");
		setWorkingPulseFrame(0);
	});

	it("retains ordered thinking, two cells, and a recoverable cell error while collapsed", () => {
		const component = new ActExecutionComponent(start());
		const events: ActProjectionEvent[] = [
			event({
				sequence: 2,
				event: "assistant_delta",
				stream: "thinking",
				text: "check namespace",
				textTruncated: false,
			}),
			event({
				sequence: 3,
				event: "assistant_delta",
				stream: "text",
				text: "running cells",
				textTruncated: false,
			}),
			event({ sequence: 4, event: "cell_start", cellId: "cell-1", code: "1 / 0", codeTruncated: false }),
			event({
				sequence: 5,
				event: "cell_terminal",
				cellId: "cell-1",
				status: "error",
				stdout: "before failure",
				stdoutTruncated: false,
				stderr: "",
				stderrTruncated: false,
				error: "ZeroDivisionError",
				resultTruncated: false,
				errorTruncated: false,
			}),
			event({ sequence: 6, event: "cell_start", cellId: "cell-2", code: "6 * 7", codeTruncated: false }),
			event({
				sequence: 7,
				event: "cell_terminal",
				cellId: "cell-2",
				status: "ok",
				stdout: "",
				stdoutTruncated: false,
				stderr: "",
				stderrTruncated: false,
				result: "42",
				resultTruncated: false,
				errorTruncated: false,
			}),
			terminal("done", 8),
		];
		for (const next of events) component.update(next);

		const collapsed = render(component);
		expect(collapsed).toMatchInlineSnapshot(
			`"   ✓ Act · Directing model → test/luna-model · done · 2 cells · 15 tokens · $0.0030"`,
		);
		expect(collapsed).not.toContain("ZeroDivisionError");
		expect(component.snapshot.cells.map((cell) => cell.scopedId)).toEqual(["act-a:cell-1", "act-a:cell-2"]);
		expect(component.snapshot.cells[0]?.status).toBe("error");
		expect(component.snapshot.status).toBe("done");
		component.update(
			event({
				sequence: 9,
				event: "assistant_delta",
				stream: "thinking",
				text: "must not appear",
				textTruncated: false,
			}),
		);
		expect(component.snapshot.sequence).toBe(8);

		component.setExpanded(true);
		const expanded = render(component);
		expect(expanded).toContain("prompt: inspect retained state");
		expect(expanded).toContain("thinking: check namespace");
		expect(expanded).toContain("text: running cells");
		expect(expanded).toContain("cell cell-1 · error: 1 / 0");
		expect(expanded).toContain("ZeroDivisionError");
		expect(expanded).toContain("cell cell-2 · ok: 6 * 7");
		expect(expanded).toContain("result: 42");
		expect(expanded.indexOf("cell cell-1")).toBeLessThan(expanded.indexOf("cell cell-2"));
		expect(expanded).toContain("usage: 15 tokens · $0.0030");
		expect(expanded).not.toContain("to steer");
		expect(expanded).not.toContain("value");
	});

	it.each(["error", "cancelled"] as const)("renders the terminal %s status and error", (status) => {
		const component = new ActExecutionComponent(start());
		component.update(terminal(status, 2, { error: `${status} detail` }));
		component.setExpanded(true);
		const output = render(component);
		expect(output).toContain(`· ${status} ·`);
		expect(output).toContain(`${status}: ${status} detail`);
	});

	it("renders a self-contained late terminal with cooperative-only cancellation", () => {
		const component = new ActExecutionComponent(
			terminal("cancelled", 9, {
				model: "windows-model",
				capability: "cooperative-only",
				error: "cancelled on Windows",
			}),
		);
		component.setExpanded(true);
		const output = render(component);
		expect(output).toContain("Directing model → test/windows-model");
		expect(output).toContain("prompt: inspect retained state");
		expect(output).toContain("cancellation: cooperative cancellation only");
		expect(output).toContain("cancelled: cancelled on Windows");
		expect(output).toContain("usage: 15 tokens · $0.0030");
	});

	it("bounds prompt and aggregate progress while preserving running controls", () => {
		const component = new ActExecutionComponent(
			event({
				sequence: 1,
				event: "start",
				prompt: "p".repeat(ACT_COMPONENT_PROMPT_MAX_CHARS + 10),
				promptTruncated: false,
				model: { provider: "test", id: "bounded-model" },
				cancellationCapability: "posix-managed",
			}),
		);
		component.update(
			event({
				sequence: 2,
				event: "assistant_delta",
				stream: "thinking",
				text: "t".repeat(ACT_COMPONENT_PROGRESS_MAX_CHARS + 10),
				textTruncated: false,
			}),
		);
		expect(component.snapshot.prompt.length).toBe(ACT_COMPONENT_PROMPT_MAX_CHARS);
		expect(component.snapshot.promptTruncated).toBe(true);
		expect(component.snapshot.progressChars).toBe(ACT_COMPONENT_PROGRESS_MAX_CHARS);
		expect(component.snapshot.progressTruncated).toBe(true);
		component.setExpanded(true);
		const output = render(component, 100);
		expect(output).toContain("additional Act progress omitted [truncated]");
		expect(output).toContain("type a message to steer");
		expect(output).toContain("to cancel");
	});

	it("bounds sequential nested Act components per root tool", () => {
		const root = new ToolExecutionComponent(
			"ipython",
			"outer-many",
			{ code: "await many_acts()" },
			{},
			undefined,
			fakeTui(),
			"/tmp",
		);
		for (let index = 0; index <= ACT_COMPONENT_MAX_ACTS_PER_TOOL; index += 1) {
			expect(
				root.appendActEvent(start(`model-${index}`, { actId: `act-${index}`, outerToolCallId: "outer-many" })),
			).toBe(true);
		}
		expect(root.getActExecutionComponents()).toHaveLength(ACT_COMPONENT_MAX_ACTS_PER_TOOL);
		expect(render(root)).toContain("additional Act executions omitted");
	});

	it("nests model-switched Acts under the correlated IPython tool through root completion", () => {
		const root = new ToolExecutionComponent(
			"ipython",
			"outer-a",
			{ code: "await rlm.act('a')" },
			{},
			undefined,
			fakeTui(),
			"/tmp",
		);
		root.markExecutionStarted();
		expect(root.appendActEvent(start("luna-model"))).toBe(true);
		expect(root.appendActEvent(terminal("done", 2))).toBe(true);
		expect(root.appendActEvent(start("deepseek-model", { actId: "act-b", outerToolCallId: "outer-a" }))).toBe(true);
		expect(root.appendActEvent(start("wrong-root", { actId: "act-x", outerToolCallId: "outer-other" }))).toBe(false);
		root.updateResult({ content: [{ type: "text", text: "root complete" }], isError: false });
		expect(
			root.appendActEvent(
				terminal("cancelled", 2, {
					actId: "act-b",
					outerToolCallId: "outer-a",
					model: "deepseek-model",
					error: "cancelled",
				}),
			),
		).toBe(true);

		const collapsed = render(root);
		expect(collapsed).toContain("test/luna-model");
		expect(collapsed).toContain("test/deepseek-model");
		expect(root.getActExecutionComponents().map((component) => component.actId)).toEqual(["act-a", "act-b"]);
		root.setExpanded(true);
		const expanded = render(root);
		expect(expanded).toContain("root complete");
		expect(expanded).toContain("cancellation: POSIX managed cancellation");
		expect(expanded).toContain("cancelled: cancelled");

		const ordinary = new ToolExecutionComponent(
			"ipython",
			"ordinary",
			{ code: "1 + 1" },
			{},
			undefined,
			fakeTui(),
			"/tmp",
		);
		ordinary.updateResult({ content: [{ type: "text", text: "2" }], isError: false });
		expect(render(ordinary)).not.toContain("Act");
	});
});
