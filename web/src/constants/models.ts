// Model constants — served by `GET /api/registry` via the registry store.
import { useRegistryStore } from "@/stores/registry";
import { isOpenAICompatibleProvider, useProviders } from "./providers";

// Capacity metadata for UI badges — icon + label + color per capability.
export const CAPACITY_META = {
	vision: {
		icon: "visibility",
		label: "Vision",
		desc: "Supports image input",
		color: "text-blue-500",
	},
	// search: temporarily hidden (feature not wired yet)
	reasoning: {
		icon: "neurology",
		label: "Reasoning",
		desc: "Supports reasoning / thinking",
		color: "text-amber-500",
	},
};

export const getModelKind = (
	m: Record<string, any> | null | undefined,
	fallback: any = null,
) => m?.kind || m?.type || fallback;

export function useModels() {
	const store = useRegistryStore();
	const { AI_PROVIDERS } = useProviders();

	// Providers that accept any model (passthrough).
	const PASSTHROUGH_PROVIDERS = new Set(
		Object.entries(AI_PROVIDERS)
			.filter(([, p]) => p.passthroughModels)
			.map(([key]) => key),
	);

	function isValidModel(aliasOrId: string, modelId: string): boolean {
		if (isOpenAICompatibleProvider(aliasOrId)) return true;
		if (PASSTHROUGH_PROVIDERS.has(aliasOrId)) return true;
		const models = store.PROVIDER_MODELS[aliasOrId];
		if (!models) return false;
		return models.some((m) => m.id === modelId);
	}

	/** `AI_MODELS` — flattened `PROVIDER_MODELS`. */
	const AI_MODELS = Object.entries(store.PROVIDER_MODELS).flatMap(
		([alias, models]) =>
			models.map((m) => ({ provider: alias, model: m.id, name: m.name })),
	);

	function getModelsByProviderId(
		providerId: string,
	): Array<Record<string, any>> {
		const alias = store.PROVIDER_ID_TO_ALIAS[providerId] || providerId;
		return store.PROVIDER_MODELS[alias] || [];
	}

	return {
		PROVIDER_MODELS: store.PROVIDER_MODELS,
		PROVIDER_ID_TO_ALIAS: store.PROVIDER_ID_TO_ALIAS,
		PASSTHROUGH_PROVIDERS,
		AI_MODELS,
		isValidModel,
		getModelsByProviderId,
	};
}
