import { beforeAll, describe, expect, it } from "vitest";
import { createScratchHandoffCloseoutMessage } from "../src/core/compaction/scratch-handoff.js";
import {
	InjectedPromptMessageComponent,
	isInjectedPromptMessage,
} from "../src/modes/interactive/components/injected-prompt-message.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";

function stripAnsi(text: string): string {
	return text.replace(/\u001b\[[0-9;]*m/g, "");
}

describe("scratch handoff TUI", () => {
	beforeAll(() => {
		initTheme();
	});

	it("renders closeout as a collapsed compaction event at 80 columns", () => {
		const message = createScratchHandoffCloseoutMessage({
			displayPath: "agent/20260904/session.org",
			phase: "create",
			content:
				"Stop working for now; please create a .org file brain-dump with nested TODO headings and enough detail for a colleague.",
		});
		const component = new InjectedPromptMessageComponent(message);
		const collapsed = stripAnsi(component.render(80).join("\n"));

		expect(message.display).toBe(true);
		expect(isInjectedPromptMessage(message)).toBe(true);
		expect(collapsed).toContain("◆");
		expect(collapsed).toContain("Scratch handoff");
		expect(collapsed).toContain("create · agent/20260904/session.org");
		expect(collapsed).toContain("to expand");
		expect(collapsed).not.toContain("brain-dump");

		component.setExpanded(true);
		expect(stripAnsi(component.render(80).join("\n"))).toContain("brain-dump");
	});
});
