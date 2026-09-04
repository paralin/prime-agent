import { describe, expect, it } from "vitest";
import { RepetitionDetector } from "../src/repetition-detector.js";

function feeds(text: string, threshold = 5, chunk = 7): boolean {
	const detector = new RepetitionDetector(threshold);
	for (let i = 0; i < text.length; i += chunk) {
		if (detector.observeText(text.slice(i, i + chunk))) return true;
	}
	return false;
}

const THOUGHT_LOOP = ` But wait — actually could a legit assistant output 6 identical lines of code? Rarely. But is it a false positive risk?
 Actually identical consecutive code lines doing the same thing is weird.

 But wait — actually could a legit assistant output 6 identical lines of code? Rarely. But is it a false positive risk?
 Actually identical consecutive code lines doing the same thing is weird.

 But wait — actually could a legit assistant output 6 identical lines of code? Rarely. But is it a false positive risk?
 Actually identical consecutive code lines doing the same thing is weird.`;

describe("RepetitionDetector", () => {
	it("detects a thought-trace paragraph loop after three copies", () => {
		expect(feeds(THOUGHT_LOOP)).toBe(true);
	});

	it("detects a long unique block repeated past the default threshold", () => {
		const block = "The parser should stop when the same unique paragraph comes back again without new evidence. ";
		expect(block.length).toBeGreaterThanOrEqual(80);
		expect(feeds(block.repeat(3))).toBe(false);
		expect(feeds(block.repeat(6))).toBe(true);
	});

	it("does not treat two restated planning sections as a loop", () => {
		const section = [
			"Executor re-verification: before each action, cheap re-checks:",
			"- delete: verify merged (merge-base --is-ancestor) if it fails, skip with a failure message.",
			"- prune: verify still junk-only? Re-scan that worktree (one status call). OK.",
			"- commit: verify branch not protected.",
			"- merge: verify main checkout clean and on default.",
			"These are cheap. Implement minimal: for delete/prune re-verify; for merge verify clean main checkout.",
			"",
			"Worktree removal: git worktree remove <path> from common dir. If dirty, it fails; use --force only when",
			"action is prune (junk-only) or when the user confirmed delete of dirty. Planner never emits delete for dirty.",
			"For prune, plain remove fails due to untracked junk; use --force after re-verify junk-only.",
			"Then branch -d if branch merged (ahead==0). Keep: also delete branch with -d (safe).",
			"",
			"Ordering: commit, push, merge, pr, delete, prune.",
		].join("\n");
		expect(section.length).toBeGreaterThan(256);
		expect(feeds(`${section}\n\n${section}\n`)).toBe(false);
	});

	it("does not treat three short sentences as a loop", () => {
		expect(feeds("The file roadmap.org is at the repo root. ".repeat(3))).toBe(false);
	});

	it("detects a short sentence repeated past the default threshold", () => {
		expect(feeds("The roadmap file lives at the repository root. ".repeat(6))).toBe(true);
	});

	it("detects six identical long lines", () => {
		const line = "    const repeatedBinding = waitForIdle(controller, timeoutMs);";
		expect(feeds(Array(6).fill(line).join("\n"))).toBe(true);
	});

	it("does not trigger on varied sentences or punctuation rules", () => {
		expect(
			feeds("The parser reads the header first. Then it validates the checksum. Finally it returns the payload. "),
		).toBe(false);
		expect(feeds("-".repeat(40))).toBe(false);
		expect(feeds("    const x = 1;\n    const y = 2;\n    const z = 3;\n")).toBe(false);
	});

	it("respects a custom short-block threshold", () => {
		const block = "repetitive block ";
		expect(feeds(block.repeat(4), 5)).toBe(false);
		expect(feeds(block.repeat(2), 2)).toBe(true);
	});
});
