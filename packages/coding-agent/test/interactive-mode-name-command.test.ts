import { Container } from "@earendil-works/pi-tui";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { formatAgentSessionNameUnavailable } from "../src/core/agent-messages.js";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";

type Context = {
	agentConnection: {
		setSessionName: (name: string) => Promise<void>;
	};
	chatContainer: Container;
	ui: { requestRender: () => void };
	showWarning: (message: string) => void;
	showError: (message: string) => void;
	getCurrentSessionName: () => string | undefined;
};

type SubmitContext = {
	defaultEditor: { onSubmit?: (text: string) => Promise<void> };
	editor: { getText: () => string; setText: (text: string) => void };
	handleNameCommand: (text: string) => Promise<void>;
	[key: string]: unknown;
};

type Prototype = {
	handleNameCommand(this: Context, text: string): Promise<void>;
	setupEditorSubmitHandler(this: SubmitContext): void;
};

const prototype = InteractiveMode.prototype as unknown as Prototype;

function renderAll(container: Container, width = 120): string {
	return container.children
		.flatMap((child) => child.render(width))
		.join("\n")
		.replace(/\u001b\[[0-9;]*m/g, "");
}

function makeContext(overrides: Partial<Context> = {}): Context {
	return {
		agentConnection: {
			setSessionName: vi.fn(async () => {}),
		},
		chatContainer: new Container(),
		ui: { requestRender: vi.fn() },
		showWarning: vi.fn(),
		showError: vi.fn(),
		getCurrentSessionName: vi.fn(() => undefined),
		...overrides,
	};
}

describe("InteractiveMode /name", () => {
	beforeAll(() => initTheme("dark"));

	it("shows a chat error when the chosen session name is already taken", async () => {
		const message = formatAgentSessionNameUnavailable("target name", 0);
		const context = makeContext({
			agentConnection: {
				setSessionName: vi.fn(async () => {
					throw new Error(message);
				}),
			},
		});

		await expect(prototype.handleNameCommand.call(context, "/name target name")).resolves.toBeUndefined();

		expect(context.agentConnection.setSessionName).toHaveBeenCalledWith("target name");
		expect(context.showError).toHaveBeenCalledWith(message);
		expect(renderAll(context.chatContainer)).not.toContain("Session name set:");
	});

	it("keeps the editor submit promise resolved so a taken name does not crash the TUI", async () => {
		const message = formatAgentSessionNameUnavailable("target name", 0);
		const context = makeContext({
			agentConnection: {
				setSessionName: vi.fn(async () => {
					throw new Error(message);
				}),
			},
		});
		const submitContext = {
			defaultEditor: {},
			editor: {
				getText: () => "",
				setText: vi.fn(),
			},
			handleNameCommand: (text: string) => prototype.handleNameCommand.call(context, text),
			submittedInputBehavior: "steer",
			inputSubmissionGeneration: 0,
			inputSubmissionsPending: 0,
			pendingPromptStashReleases: [],
			promptStashState: {},
			clearShortcutGuide: vi.fn(),
		} as unknown as SubmitContext;
		prototype.setupEditorSubmitHandler.call(submitContext);

		await expect(submitContext.defaultEditor.onSubmit?.("/name target name")).resolves.toBeUndefined();
		expect(context.showError).toHaveBeenCalledWith(message);
	});
});
