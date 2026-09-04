import type { Api, Model, OpenAICompletionsCompat, ThinkingLevel, ThinkingLevelMap } from "@earendil-works/pi-ai";

const MERGE_GATEWAY_CATALOG_BASE_URL = "https://api-gateway.merge.dev/v1";
const MERGE_GATEWAY_CHAT_BASE_URL = "https://api-gateway.merge.dev/v1/ai-sdk";
const MERGE_GATEWAY_API: Api = "openai-completions";
const MERGE_GATEWAY_COMPAT: OpenAICompletionsCompat = {
	reasoningField: "thinking",
	requireFinishReason: true,
	supportsStore: true,
	supportsDeveloperRole: false,
	supportsReasoningEffort: true,
	maxTokensField: "max_tokens",
	thinkingFormat: "merge",
	sendSessionAffinityHeaders: ["x-session-affinity", "X-Session-Id"],
};
const DEFAULT_CONTEXT_WINDOW = 128_000;
const DEFAULT_MAX_TOKENS = 16_384;
const MERGE_GATEWAY_PAGE_LIMIT = 500;

interface CatalogModel {
	id: string;
	name?: string;
	reasoning?: boolean;
	input?: Model<"openai-completions">["input"];
	contextWindow?: number;
	maxTokens?: number;
	compat?: {
		supportsReasoningEffort: boolean;
		supportsReasoningDisable: boolean;
	};
	supportedEfforts?: ThinkingLevel[];
}

interface CatalogVendor {
	availability_status?: unknown;
	context_window?: unknown;
	max_output_tokens?: unknown;
	capabilities?: {
		input?: unknown;
		output?: unknown;
		supports_tool_calling?: unknown;
		supports_reasoning?: unknown;
		reasoning?: {
			disable_supported?: unknown;
			controls?: unknown;
			effort_values?: unknown;
		};
	};
}

const THINKING_LEVELS: readonly ThinkingLevel[] = ["minimal", "low", "medium", "high", "xhigh", "max"];

function commonEffortValues(vendors: CatalogVendor[]): ThinkingLevel[] | undefined {
	if (
		vendors.some(
			(vendor) =>
				!Array.isArray(vendor.capabilities?.reasoning?.controls) ||
				!vendor.capabilities.reasoning.controls.includes("reasoning_effort") ||
				!Array.isArray(vendor.capabilities.reasoning.effort_values),
		)
	) {
		return undefined;
	}
	const common = THINKING_LEVELS.filter((level) =>
		vendors.every((vendor) => (vendor.capabilities?.reasoning?.effort_values as unknown[]).includes(level)),
	);
	return common.length > 0 ? common : undefined;
}

function routeThinkingLevelMap(
	base: ThinkingLevelMap | undefined,
	supportedEfforts: ThinkingLevel[] | undefined,
	supportsDisable: boolean | undefined,
): ThinkingLevelMap | undefined {
	if (!base && supportedEfforts === undefined && supportsDisable !== false) return undefined;
	const result: ThinkingLevelMap = { ...(base ?? {}) };
	if (supportsDisable === false && result.off === undefined) result.off = null;
	if (supportedEfforts !== undefined) {
		const supported = new Set<string>(supportedEfforts);
		for (const level of THINKING_LEVELS) {
			const mapped = base?.[level] ?? level;
			result[level] = mapped !== null && supported.has(mapped) ? mapped : null;
		}
		if (result.off && !supported.has(result.off)) result.off = null;
	}
	return result;
}

function readCatalog(value: unknown): CatalogModel[] {
	if (!value || typeof value !== "object" || !("data" in value) || !Array.isArray(value.data)) {
		throw new Error("Merge Gateway model catalog response is invalid");
	}
	return value.data.flatMap((entry): CatalogModel[] => {
		if (!entry || typeof entry !== "object") {
			throw new Error("Merge Gateway model catalog response is invalid");
		}
		// The original catalog used OpenAI-style {id} entries. Keep accepting it
		// while Merge Gateway rolls out the richer model/vendor schema.
		if ("id" in entry && typeof entry.id === "string") return [{ id: entry.id }];
		if (!("model" in entry) || typeof entry.model !== "string" || !("vendors" in entry)) {
			throw new Error("Merge Gateway model catalog response is invalid");
		}
		if (!entry.vendors || typeof entry.vendors !== "object" || Array.isArray(entry.vendors)) {
			throw new Error("Merge Gateway model catalog response is invalid");
		}
		const vendors = Object.entries(entry.vendors as Record<string, CatalogVendor>).filter(([, vendor]) => {
			const capabilities = vendor?.capabilities;
			return (
				vendor?.availability_status !== "unavailable" &&
				capabilities !== undefined &&
				capabilities.supports_tool_calling === true &&
				Array.isArray(capabilities.input) &&
				capabilities.input.includes("text") &&
				Array.isArray(capabilities.output) &&
				capabilities.output.includes("text")
			);
		});
		if (vendors.length === 0) return [];
		const routes = vendors.map(([, vendor]) => vendor);
		const supportsReasoning = routes.every((vendor) => {
			const controls = vendor.capabilities?.reasoning?.controls;
			return (
				vendor.capabilities?.supports_reasoning === true &&
				Array.isArray(controls) &&
				(controls.includes("thinking") || controls.includes("thinking.budget_tokens"))
			);
		});
		const supportsDisable =
			supportsReasoning && routes.every((vendor) => vendor.capabilities?.reasoning?.disable_supported === true);
		const supportedEfforts = supportsReasoning ? commonEffortValues(routes) : undefined;
		const finiteMinimum = (values: unknown[]): number | undefined => {
			const numbers = values.filter(
				(candidate): candidate is number => typeof candidate === "number" && candidate > 0,
			);
			return numbers.length > 0 ? Math.min(...numbers) : undefined;
		};
		return [
			{
				id: entry.model,
				name: "display_name" in entry && typeof entry.display_name === "string" ? entry.display_name : undefined,
				reasoning: supportsReasoning,
				input: routes.every(
					(vendor) => Array.isArray(vendor.capabilities?.input) && vendor.capabilities.input.includes("image"),
				)
					? ["text", "image"]
					: ["text"],
				contextWindow: finiteMinimum(routes.map((vendor) => vendor.context_window)),
				maxTokens: finiteMinimum(routes.map((vendor) => vendor.max_output_tokens)),
				compat: {
					supportsReasoningEffort: supportedEfforts !== undefined,
					supportsReasoningDisable: supportsDisable,
				},
				...(supportedEfforts ? { supportedEfforts } : {}),
			},
		];
	});
}

/** GLM-5.3-Flash accepts only low, high, and max. */
const GLM_53_FLASH_THINKING_LEVEL_MAP: NonNullable<Model<Api>["thinkingLevelMap"]> = {
	off: null,
	minimal: null,
	low: "low",
	medium: null,
	high: "high",
	xhigh: null,
	max: "max",
};

function glm53FlashThinkingLevelMap(id: string): Model<Api>["thinkingLevelMap"] | undefined {
	const normalized = id.toLowerCase();
	if (!normalized.includes("glm-5.3-flash") && !normalized.includes("glm-5-3-flash")) return undefined;
	return { ...GLM_53_FLASH_THINKING_LEVEL_MAP };
}

function modelName(id: string): string {
	return id
		.split("/")
		.at(-1)!
		.split("-")
		.map((part) => (part.length > 0 ? part[0]!.toUpperCase() + part.slice(1) : part))
		.join(" ");
}

export async function fetchMergeGatewayModels(
	apiKey: string,
	knownModels: readonly Model<Api>[],
	fetchFn: typeof fetch = fetch,
): Promise<Model<Api>[]> {
	const catalog: CatalogModel[] = [];
	const seenCursors = new Set<string>();
	let cursor: string | undefined;
	while (true) {
		const params = new URLSearchParams({ limit: String(MERGE_GATEWAY_PAGE_LIMIT) });
		if (cursor) params.set("cursor", cursor);
		const response = await fetchFn(`${MERGE_GATEWAY_CATALOG_BASE_URL}/models?${params}`, {
			headers: { Authorization: `Bearer ${apiKey}` },
			signal: AbortSignal.timeout(5_000),
		});
		if (!response.ok) {
			throw new Error(`Merge Gateway model discovery failed with HTTP ${response.status}`);
		}
		const page: unknown = await response.json();
		catalog.push(...readCatalog(page));
		if (!page || typeof page !== "object" || !("has_more" in page) || page.has_more !== true) {
			break;
		}
		const nextCursor = "next_cursor" in page && typeof page.next_cursor === "string" ? page.next_cursor.trim() : "";
		if (!nextCursor) {
			break;
		}
		if (seenCursors.has(nextCursor)) {
			throw new Error("Merge Gateway model catalog response repeated its pagination cursor");
		}
		seenCursors.add(nextCursor);
		cursor = nextCursor;
	}
	return catalog.map((entry) => {
		const id = entry.id;
		const known =
			knownModels.find((model) => model.id === id && model.provider === "merge-gateway") ??
			knownModels.find((model) => model.id === id);
		const knownMergeModel = known?.provider === "merge-gateway" ? known : undefined;
		const baseThinkingLevelMap = knownMergeModel?.thinkingLevelMap
			? { ...knownMergeModel.thinkingLevelMap }
			: glm53FlashThinkingLevelMap(id);
		const thinkingLevelMap = entry.compat
			? entry.reasoning
				? routeThinkingLevelMap(baseThinkingLevelMap, entry.supportedEfforts, entry.compat.supportsReasoningDisable)
				: undefined
			: baseThinkingLevelMap;
		return {
			id,
			name: entry.name ?? known?.name ?? modelName(id),
			api: MERGE_GATEWAY_API,
			provider: "merge-gateway",
			baseUrl: MERGE_GATEWAY_CHAT_BASE_URL,
			compat: {
				...MERGE_GATEWAY_COMPAT,
				...(entry.compat ? { supportsReasoningEffort: entry.compat.supportsReasoningEffort } : {}),
			},
			...(thinkingLevelMap ? { thinkingLevelMap } : {}),
			reasoning: entry.reasoning ?? known?.reasoning ?? true,
			input: entry.input ?? (known ? [...known.input] : ["text"]),
			cost: known ? { ...known.cost } : { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: entry.contextWindow ?? known?.contextWindow ?? DEFAULT_CONTEXT_WINDOW,
			maxTokens: entry.maxTokens ?? known?.maxTokens ?? DEFAULT_MAX_TOKENS,
		};
	});
}
