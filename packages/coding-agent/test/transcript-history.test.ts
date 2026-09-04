import type { UserMessage } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { SessionManager } from "../src/core/session-manager.js";
import { InjectedPromptMessageComponent } from "../src/modes/interactive/components/injected-prompt-message.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";
import { buildTranscriptHistory } from "../src/modes/interactive/transcript-history.js";

function user(content: string, timestamp: number): UserMessage {
	return { role: "user", content, timestamp };
}

describe("scratch transcript history", () => {
	it("keeps both generations scrollable without expanding model context", () => {
		const session = SessionManager.inMemory();
		const original = user("Original request", 1);
		session.appendMessage(original);
		for (let generation = 1; generation <= 2; generation++) {
			session.appendMessageCompaction(
				user(
					`<scratch-handoff-file path="agent/work.org">\n* TODO Generation ${generation}\n</scratch-handoff-file>\nInternal continuation instructions`,
					generation + 1,
				),
				{ summary: "", tokensBefore: 100000, details: { scratchHandoff: { version: 1, path: "agent/work.org" } } },
			);
		}
		const context = session.buildSessionContext();
		expect(context.messages).toHaveLength(1);
		const messages = buildTranscriptHistory(session.getTree(), session.getLeafId(), context.messages);
		expect(messages).toHaveLength(3);
		expect(messages[0]).toEqual(original);
		expect(messages.slice(1).map((message) => message.role)).toEqual(["custom", "custom"]);
		expect(JSON.stringify(messages)).not.toContain("Internal continuation instructions");
		expect(session.buildSessionContext()).toEqual(context);
		initTheme();
		const marker = messages[2];
		if (marker.role !== "custom") throw new Error("Expected display marker");
		for (const width of [40, 80, 248]) {
			const component = new InjectedPromptMessageComponent(marker);
			const collapsed = component.render(width).join("\n");
			expect(collapsed).toContain("Scratch compacted");
			expect(collapsed).not.toContain("Generation 2");
			component.setExpanded(true);
			expect(component.render(width).join("\n")).toContain("Generation 2");
		}
	});

	it("selects only the active branch and retains new unpersisted messages", () => {
		const session = SessionManager.inMemory();
		const root = session.appendMessage(user("root", 1));
		session.appendMessage(user("other branch", 2));
		session.branch(root);
		const selected = user("selected branch", 3);
		session.appendMessage(selected);
		const pending = user("new message", 4);
		const messages = buildTranscriptHistory(session.getTree(), session.getLeafId(), [selected, pending]);
		expect(messages).toEqual([user("root", 1), selected, pending]);
	});

	it("does not treat user-authored handoff markup as a compaction", () => {
		const session = SessionManager.inMemory();
		const quoted = user('<scratch-handoff-file path="example">\nquoted\n</scratch-handoff-file>', 1);
		session.appendMessage(quoted);
		expect(buildTranscriptHistory(session.getTree(), session.getLeafId(), [quoted])).toEqual([quoted]);
		expect(buildTranscriptHistory([], null, [quoted])).toEqual([quoted]);
	});
});
