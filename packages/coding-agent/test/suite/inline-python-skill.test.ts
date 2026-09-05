import { resolve } from "node:path";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import { loadSkillsFromDir } from "../../src/core/skills.js";
import { createTestResourceLoader } from "../utilities.js";
import { createHarness, getUserTexts } from "./harness.js";

describe("explicit Python skill invocation", () => {
	for (const ipython of [true, false]) {
		it(`identifies the import with IPython ${ipython ? "active" : "absent"}`, async () => {
			const { skills } = loadSkillsFromDir({ dir: resolve("test/fixtures/skills/python-skill"), source: "path" });
			skills[0].disableModelInvocation = true;
			const harness = await createHarness({
				resourceLoader: createTestResourceLoader({ skills }),
				tools: ipython
					? [
							{
								name: "ipython",
								label: "Python",
								description: "Fixture Python tool",
								parameters: Type.Object({}),
								execute: async () => ({ content: [{ type: "text", text: "ok" }], details: {} }),
							},
						]
					: [],
			});
			try {
				await harness.session.prompt("/skill:python-skill inspect the current work");
				const text = getUserTexts(harness).join("\n");
				expect(text).toContain("Python module: `python_skill`");
				expect(text).toContain(ipython ? "configured for preload" : "no kernel preload is available");
				expect(text).toContain("inspect the current work");
			} finally {
				harness.cleanup();
			}
		});
	}
});
