import type { Api, Model, OpenAICompletionsCompat } from "@earendil-works/pi-ai";

const MERGE_GATEWAY_BASE_URL = "https://api-gateway.merge.dev/v1";
const MERGE_GATEWAY_COMPAT: OpenAICompletionsCompat = { supportsReasoningEffort: false };
const DEFAULT_CONTEXT_WINDOW = 128_000;
const DEFAULT_MAX_TOKENS = 16_384;

interface CatalogModel {
	id: string;
	name?: string;
	reasoning?: boolean;
	input?: Model<"openai-completions">["input"];
	contextWindow?: number;
	maxTokens?: number;
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
	};
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
		const vendors = Object.values(entry.vendors as Record<string, CatalogVendor>).filter((vendor) => {
			const capabilities = vendor?.capabilities;
			return (
				vendor?.availability_status !== "unavailable" &&
				capabilities !== undefined &&
				capabilities.supports_tool_calling !== false &&
				Array.isArray(capabilities.input) &&
				capabilities.input.includes("text") &&
				Array.isArray(capabilities.output) &&
				capabilities.output.includes("text")
			);
		});
		if (vendors.length === 0) return [];
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
				reasoning: vendors.some((vendor) => vendor.capabilities?.supports_reasoning === true),
				input: vendors.some(
					(vendor) => Array.isArray(vendor.capabilities?.input) && vendor.capabilities.input.includes("image"),
				)
					? ["text", "image"]
					: ["text"],
				contextWindow: finiteMinimum(vendors.map((vendor) => vendor.context_window)),
				maxTokens: finiteMinimum(vendors.map((vendor) => vendor.max_output_tokens)),
			},
		];
	});
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
	const response = await fetchFn(`${MERGE_GATEWAY_BASE_URL}/models`, {
		headers: { Authorization: `Bearer ${apiKey}` },
		signal: AbortSignal.timeout(5_000),
	});
	if (!response.ok) {
		throw new Error(`Merge Gateway model discovery failed with HTTP ${response.status}`);
	}

	const catalog = readCatalog(await response.json());
	return catalog.map((entry) => {
		const id = entry.id;
		const known =
			knownModels.find((model) => model.id === id && model.provider === "merge-gateway") ??
			knownModels.find((model) => model.id === id);
		const api = known?.provider === "merge-gateway" ? known.api : "openai-completions";
		return {
			id,
			name: entry.name ?? known?.name ?? modelName(id),
			api,
			provider: "merge-gateway",
			baseUrl: MERGE_GATEWAY_BASE_URL,
			...(api === "openai-completions" ? { compat: MERGE_GATEWAY_COMPAT } : {}),
			reasoning: entry.reasoning ?? known?.reasoning ?? true,
			input: entry.input ?? (known ? [...known.input] : ["text"]),
			cost: known ? { ...known.cost } : { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: entry.contextWindow ?? known?.contextWindow ?? DEFAULT_CONTEXT_WINDOW,
			maxTokens: entry.maxTokens ?? known?.maxTokens ?? DEFAULT_MAX_TOKENS,
		};
	});
}
