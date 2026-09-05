import { describe, expect, it } from "vitest";
import { formatHarnessStateForPrompt, type HarnessState } from "../src/core/refinement/refinement.js";

describe("saved instruction qualifications", () => {
	it("keeps complete short entries and omits long instructions with a retrieval route", () => {
		const content = `Never run the operation. ${"Supporting detail. ".repeat(20)}Unless the user explicitly approves it.`;
		const state: HarnessState = {
			schema: 1,
			entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
			refinements: [],
		};
		state.entries.memory.rule = {
			id: "rule",
			kind: "memory",
			title: "Operation clearance",
			content,
			path: "operations",
			scope: "local",
			reference: {},
			arguments: {},
			metadata: {},
			source: "agent",
			created_at: "",
			updated_at: "",
			version: 1,
		};
		const prompt = formatHarnessStateForPrompt(state);
		expect(prompt).toContain("[local:rule]");
		expect(prompt).toContain("rlm.harness.get(kind, 'scope:id')");
		expect(prompt).toContain("[omitted:");
		expect(prompt).not.toContain("Never run the operation.");
		state.entries.memory.rule.content = "Run only with explicit user approval.";
		expect(formatHarnessStateForPrompt(state)).toContain("Run only with explicit user approval.");
	});
});
