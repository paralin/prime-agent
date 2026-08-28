import { mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Api, Model } from "@earendil-works/pi-ai";
import { describe, expect, test, vi } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { fetchMergeGatewayModels } from "../src/core/merge-gateway-models.js";
import { ModelRegistry } from "../src/core/model-registry.js";

const knownModel: Model<Api> = {
	id: "z-ai/glm-5.3",
	name: "GLM 5.3",
	api: "openai-completions",
	provider: "openrouter",
	baseUrl: "https://openrouter.ai/api/v1",
	reasoning: true,
	input: ["text", "image"],
	cost: { input: 1, output: 2, cacheRead: 0.1, cacheWrite: 0 },
	contextWindow: 262_144,
	maxTokens: 65_536,
};

describe("Merge Gateway model discovery", () => {
	test("lists every catalog model and reuses known metadata", async () => {
		const fetchFn = vi.fn(
			async () =>
				new Response(
					JSON.stringify({
						data: [{ id: "z-ai/glm-5.3" }, { id: "z-ai/glm-5.3-flash" }],
					}),
					{ status: 200, headers: { "content-type": "application/json" } },
				),
		);

		const models = await fetchMergeGatewayModels("merge-key", [knownModel], fetchFn);

		expect(fetchFn).toHaveBeenCalledWith(
			"https://api-gateway.merge.dev/v1/models",
			expect.objectContaining({
				headers: { Authorization: "Bearer merge-key" },
			}),
		);
		expect(models.map((model) => model.id)).toEqual(["z-ai/glm-5.3", "z-ai/glm-5.3-flash"]);
		expect(models[0]).toMatchObject({
			provider: "merge-gateway",
			name: "GLM 5.3",
			contextWindow: 262_144,
			maxTokens: 65_536,
			input: ["text", "image"],
		});
		expect(models[1]).toMatchObject({
			provider: "merge-gateway",
			name: "Glm 5.3 Flash",
			contextWindow: 128_000,
			maxTokens: 16_384,
		});
	});

	test("keeps the Responses API and Merge pricing from the bootstrap model", async () => {
		const bootstrap: Model<Api> = {
			id: "zai/glm-5.3-flash",
			name: "GLM 5.3 Flash",
			api: "openai-responses",
			provider: "merge-gateway",
			baseUrl: "https://api-gateway.merge.dev/v1",
			compat: {
				sendSessionIdHeader: false,
				supportsStore: false,
				supportsReasoning: false,
				supportsDeveloperRole: false,
				supportsTools: false,
			},
			reasoning: true,
			input: ["text", "image"],
			cost: { input: 0.015, output: 0.05, cacheRead: 0, cacheWrite: 0 },
			contextWindow: 1_000_000,
			maxTokens: 131_000,
		};
		const fetchFn = vi.fn(async () => new Response(JSON.stringify({ data: [{ id: "zai/glm-5.3-flash" }] })));

		const [model] = await fetchMergeGatewayModels("merge-key", [bootstrap], fetchFn);

		expect(model).toMatchObject({
			api: "openai-responses",
			compat: {
				sendSessionIdHeader: false,
				supportsStore: false,
				supportsReasoning: false,
				supportsDeveloperRole: false,
				supportsTools: false,
			},
			cost: { input: 0.015, output: 0.05 },
		});
	});

	test("rejects malformed catalogs", async () => {
		const fetchFn = vi.fn(async () => new Response(JSON.stringify({ models: [] }), { status: 200 }));
		await expect(fetchMergeGatewayModels("merge-key", [], fetchFn)).rejects.toThrow(
			"Merge Gateway model catalog response is invalid",
		);
	});
	test("reads the current vendor catalog and keeps tool-capable text models", async () => {
		const fetchFn = vi.fn(
			async () =>
				new Response(
					JSON.stringify({
						data: [
							{
								model: "z-ai/glm-5.3",
								display_name: "GLM 5.3 Live",
								vendors: {
									fast: {
										availability_status: "available",
										context_window: 262_144,
										max_output_tokens: 65_536,
										capabilities: {
											input: ["text", "image"],
											output: ["text"],
											supports_tool_calling: true,
											supports_reasoning: true,
										},
									},
								},
							},
							{
								model: "zai/glm-5.3-flash",
								display_name: "GLM 5.3 Flash",
								vendors: {
									fast: {
										availability_status: "available",
										capabilities: { input: ["text"], output: ["text"] },
									},
								},
							},
							{
								model: "ai21/jamba-1-5-large",
								vendors: {
									bedrock: {
										capabilities: { input: ["text"], output: ["text"], supports_tool_calling: false },
									},
								},
							},
						],
					}),
				),
		);

		const models = await fetchMergeGatewayModels("merge-key", [], fetchFn);

		expect(models).toHaveLength(2);
		expect(models[0]).toMatchObject({
			id: "z-ai/glm-5.3",
			name: "GLM 5.3 Live",
			reasoning: true,
			input: ["text", "image"],
			contextWindow: 262_144,
			maxTokens: 65_536,
		});
		expect(models[1]).toMatchObject({ id: "zai/glm-5.3-flash", name: "GLM 5.3 Flash" });
	});
	test("adds authenticated catalog models to the registry", async () => {
		const directory = join(tmpdir(), `merge-gateway-models-${crypto.randomUUID()}`);
		mkdirSync(directory, { recursive: true });
		const fetchFn = vi.fn(
			async () =>
				new Response(JSON.stringify({ data: [{ id: "z-ai/glm-5.3-flash" }] }), {
					status: 200,
				}),
		);
		vi.stubGlobal("fetch", fetchFn);
		try {
			const authStorage = AuthStorage.create(join(directory, "auth.json"));
			authStorage.setRuntimeApiKey("merge-gateway", "merge-key");
			const registry = ModelRegistry.create(authStorage, join(directory, "models.json"));

			const models = await registry.refreshAvailableModels();

			const mergeGatewayIds = models.filter((model) => model.provider === "merge-gateway").map((model) => model.id);
			expect(mergeGatewayIds).toContain("z-ai/glm-5.3-flash");
			expect(mergeGatewayIds).toContain("zai/glm-5.3-flash");
		} finally {
			vi.unstubAllGlobals();
			rmSync(directory, { recursive: true, force: true });
		}
	});
});
