import { mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type Api, type Model, streamSimple } from "@earendil-works/pi-ai";
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
	test("keeps native effort controls without sending unsupported thinking budgets", async () => {
		const fetchFn = vi.fn(
			async () =>
				new Response(
					JSON.stringify({
						data: [
							{
								model: "example/native-reasoning",
								vendors: {
									native: {
										capabilities: {
											input: ["text"],
											output: ["text"],
											supports_tool_calling: true,
											supports_reasoning: true,
											reasoning: {
												controls: ["reasoning_effort"],
												effort_values: ["low", "high"],
												disable_supported: false,
											},
										},
									},
								},
							},
						],
					}),
				),
		);
		const [model] = await fetchMergeGatewayModels("merge-key", [], fetchFn);
		expect(model).toMatchObject({
			reasoning: true,
			compat: { supportsReasoningEffort: true, thinkingFormat: "openai" },
		});
		let payload: unknown;
		await streamSimple(
			model,
			{ messages: [{ role: "user", content: "hello", timestamp: 0 }] },
			{
				apiKey: "offline-test",
				reasoning: "low",
				thinkingBudgets: { low: 1024 },
				onPayload(value) {
					payload = value;
					throw new Error("offline capture");
				},
			},
		).result();
		expect(payload).toMatchObject({ reasoning_effort: "low" });
		expect(payload).not.toHaveProperty("thinking");
	});

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
			"https://api-gateway.merge.dev/v1/models?limit=500",
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

	test("uses the OpenCode Chat route and keeps Merge pricing from the bootstrap model", async () => {
		const bootstrap: Model<Api> = {
			id: "zai/glm-5.3-flash",
			name: "GLM 5.3 Flash",
			api: "openai-completions",
			provider: "merge-gateway",
			baseUrl: "https://api-gateway.merge.dev/v1/ai-sdk",
			reasoning: true,
			input: ["text", "image"],
			cost: { input: 0.015, output: 0.05, cacheRead: 0, cacheWrite: 0 },
			contextWindow: 1_000_000,
			maxTokens: 131_000,
		};
		const fetchFn = vi.fn(async () => new Response(JSON.stringify({ data: [{ id: "zai/glm-5.3-flash" }] })));

		const [model] = await fetchMergeGatewayModels("merge-key", [bootstrap], fetchFn);

		expect(model).toMatchObject({
			api: "openai-completions",
			baseUrl: "https://api-gateway.merge.dev/v1/ai-sdk",
			compat: {
				reasoningField: "thinking",
				requireFinishReason: true,
				supportsStore: true,
				supportsDeveloperRole: false,
				maxTokensField: "max_tokens",
				thinkingFormat: "merge",
				sendSessionAffinityHeaders: ["x-session-affinity", "X-Session-Id"],
			},
			cost: { input: 0.015, output: 0.05 },
		});
	});

	test("keeps only the GLM-5.3-Flash thinking levels supported by Merge", async () => {
		const bootstrap: Model<Api> = {
			id: "zai/glm-5.3-flash",
			name: "GLM 5.3 Flash",
			api: "openai-completions",
			provider: "merge-gateway",
			baseUrl: "https://api-gateway.merge.dev/v1/ai-sdk",
			reasoning: true,
			thinkingLevelMap: {
				off: null,
				minimal: null,
				low: "low",
				medium: null,
				high: "high",
				xhigh: null,
				max: "max",
			},
			input: ["text", "image"],
			cost: { input: 0.015, output: 0.05, cacheRead: 0, cacheWrite: 0 },
			contextWindow: 1_000_000,
			maxTokens: 131_000,
		};
		const fetchFn = vi.fn(async () => new Response(JSON.stringify({ data: [{ id: "zai/glm-5.3-flash" }] })));

		const [model] = await fetchMergeGatewayModels("merge-key", [bootstrap], fetchFn);

		expect(model.thinkingLevelMap).toEqual({
			off: null,
			minimal: null,
			low: "low",
			medium: null,
			high: "high",
			xhigh: null,
			max: "max",
		});
	});

	test("recognizes Merge's live GLM thinking control name", async () => {
		const fetchFn = vi.fn(
			async () =>
				new Response(
					JSON.stringify({
						data: [
							{
								model: "zai/glm-5.3-flash",
								vendors: {
									particle: {
										capabilities: {
											input: ["text", "image"],
											output: ["text", "tool_use"],
											supports_tool_calling: true,
											supports_reasoning: true,
											reasoning: {
												disable_supported: false,
												controls: ["reasoning_effort", "thinking"],
												effort_values: ["low", "high", "max"],
											},
										},
									},
								},
							},
						],
					}),
				),
		);

		const [model] = await fetchMergeGatewayModels("merge-key", [], fetchFn);

		expect(model).toMatchObject({
			reasoning: true,
			compat: {
				reasoningField: "thinking",
				requireFinishReason: true,
				supportsReasoningEffort: true,
				maxTokensField: "max_tokens",
				thinkingFormat: "merge",
				sendSessionAffinityHeaders: ["x-session-affinity", "X-Session-Id"],
			},
			thinkingLevelMap: {
				off: null,
				minimal: null,
				low: "low",
				medium: null,
				high: "high",
				xhigh: null,
				max: "max",
			},
		});
	});

	test("applies the GLM-5.3-Flash thinking map when the catalog has no bootstrap map", async () => {
		const fetchFn = vi.fn(async () => new Response(JSON.stringify({ data: [{ id: "zai/glm-5.3-flash" }] })));

		const [model] = await fetchMergeGatewayModels("merge-key", [], fetchFn);

		expect(model.thinkingLevelMap).toEqual({
			off: null,
			minimal: null,
			low: "low",
			medium: null,
			high: "high",
			xhigh: null,
			max: "max",
		});
	});

	test("pages the catalog while has_more is true", async () => {
		const fetchFn = vi
			.fn()
			.mockResolvedValueOnce(
				new Response(
					JSON.stringify({
						data: [{ id: "first/model" }],
						has_more: true,
						next_cursor: "first/model",
					}),
				),
			)
			.mockResolvedValueOnce(
				new Response(
					JSON.stringify({
						data: [{ id: "second/model" }],
						has_more: false,
					}),
				),
			);

		const models = await fetchMergeGatewayModels("merge-key", [], fetchFn);

		expect(fetchFn).toHaveBeenNthCalledWith(
			1,
			"https://api-gateway.merge.dev/v1/models?limit=500",
			expect.objectContaining({ headers: { Authorization: "Bearer merge-key" } }),
		);
		expect(fetchFn).toHaveBeenNthCalledWith(
			2,
			"https://api-gateway.merge.dev/v1/models?limit=500&cursor=first%2Fmodel",
			expect.objectContaining({ headers: { Authorization: "Bearer merge-key" } }),
		);
		expect(models.map((model) => model.id)).toEqual(["first/model", "second/model"]);
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
											reasoning: {
												disable_supported: true,
												controls: ["thinking.budget_tokens", "reasoning_effort"],
												effort_values: ["low", "medium", "high"],
											},
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
										capabilities: {
											input: ["text"],
											output: ["text"],
											supports_tool_calling: true,
										},
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
		expect(models[1]).toMatchObject({
			id: "zai/glm-5.3-flash",
			name: "GLM 5.3 Flash",
			reasoning: false,
			input: ["text"],
		});
	});

	test("advertises only capabilities shared by every eligible vendor route", async () => {
		const fetchFn = vi.fn(
			async () =>
				new Response(
					JSON.stringify({
						data: [
							{
								model: "example/shared-model",
								vendors: {
									vision: {
										capabilities: {
											input: ["text", "image"],
											output: ["text"],
											supports_tool_calling: true,
											supports_reasoning: true,
											reasoning: {
												disable_supported: true,
												controls: ["thinking.budget_tokens", "reasoning_effort"],
												effort_values: ["low", "high"],
											},
										},
									},
									text: {
										capabilities: {
											input: ["text"],
											output: ["text"],
											supports_tool_calling: true,
											supports_reasoning: true,
											reasoning: {
												disable_supported: false,
												controls: ["thinking.budget_tokens"],
											},
										},
									},
									no_tools: {
										capabilities: {
											input: ["text", "image"],
											output: ["text"],
											supports_tool_calling: false,
										},
									},
								},
							},
						],
					}),
				),
		);

		const [model] = await fetchMergeGatewayModels("merge-key", [], fetchFn);

		expect(model).toMatchObject({
			reasoning: true,
			input: ["text"],
			compat: {
				reasoningField: "thinking",
				requireFinishReason: true,
				supportsReasoningEffort: false,
				maxTokensField: "max_tokens",
				thinkingFormat: "merge",
				sendSessionAffinityHeaders: ["x-session-affinity", "X-Session-Id"],
			},
			thinkingLevelMap: { off: null },
		});
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
