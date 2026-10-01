// Provider definitions — served by `GET /api/registry` via the registry store.
// The store is loaded before the router mounts, so every accessor reads
// synchronously.
import { useRegistryStore } from "@/stores/registry";

// Thinking config definitions
// options: list of selectable modes ("auto" = no override from server)
// defaultMode: fallback when user hasn't configured
// extended: claude-style thinking (thinking.type + budget_tokens) — most providers
// effort: openai-style reasoning_effort — only openai + codex
export const THINKING_CONFIG = {
	extended: {
		options: ["auto", "on", "off"],
		defaultMode: "auto",
		defaultBudgetTokens: 10000,
	},
	effort: {
		options: ["auto", "none", "low", "medium", "high"],
		defaultMode: "auto",
	},
};

export const OPENAI_COMPATIBLE_PREFIX = "openai-compatible-";
export const ANTHROPIC_COMPATIBLE_PREFIX = "anthropic-compatible-";
export const CUSTOM_EMBEDDING_PREFIX = "custom-embedding-";

export function isOpenAICompatibleProvider(providerId: unknown): boolean {
	return (
		typeof providerId === "string" &&
		providerId.startsWith(OPENAI_COMPATIBLE_PREFIX)
	);
}

export function isAnthropicCompatibleProvider(providerId: unknown): boolean {
	return (
		typeof providerId === "string" &&
		providerId.startsWith(ANTHROPIC_COMPATIBLE_PREFIX)
	);
}

export function isCustomEmbeddingProvider(providerId: unknown): boolean {
	return (
		typeof providerId === "string" &&
		providerId.startsWith(CUSTOM_EMBEDDING_PREFIX)
	);
}

export const AUTH_METHODS = {
	oauth: { id: "oauth", name: "OAuth" },
	apikey: { id: "apikey", name: "API Key" },
	cookie: { id: "cookie", name: "Cookie" },
};

export function useProviders() {
	const store = useRegistryStore();

	function getProviderByAlias(alias: string): Record<string, any> | null {
		for (const provider of Object.values(store.AI_PROVIDERS)) {
			if (provider.alias === alias || provider.id === alias) return provider;
		}
		return null;
	}

	function resolveProviderId(aliasOrId: string): string {
		const provider = getProviderByAlias(aliasOrId);
		return provider?.id || aliasOrId;
	}

	function getProviderAlias(providerId: string): string {
		const provider = store.AI_PROVIDERS[providerId];
		return provider?.alias || providerId;
	}

	/** Providers serving a service kind (e.g. "tts", "embedding"). */
	function getProvidersByKind(kind: string): Array<Record<string, any>> {
		return Object.values(store.AI_PROVIDERS)
			.filter((p) => {
				const kinds = p.serviceKinds ?? ["llm"];
				if (!kinds.includes(kind)) return false;
				if (p.hidden) return false;
				if (p.hiddenKinds?.includes(kind)) return false;
				return true;
			})
			.sort(
				(a, b) =>
					(a.priority ?? a.mediaPriority ?? 999) -
					(b.priority ?? b.mediaPriority ?? 999),
			);
	}

	const ALIAS_TO_ID: Record<string, string> = {};
	const ID_TO_ALIAS: Record<string, string> = {};
	for (const p of Object.values(store.AI_PROVIDERS)) {
		ALIAS_TO_ID[p.alias] = p.id;
		ID_TO_ALIAS[p.id] = p.alias;
	}

	return {
		AI_PROVIDERS: store.AI_PROVIDERS,
		FREE_PROVIDERS: store.FREE_PROVIDERS,
		FREE_TIER_PROVIDERS: store.FREE_TIER_PROVIDERS,
		OAUTH_PROVIDERS: store.OAUTH_PROVIDERS,
		APIKEY_PROVIDERS: store.APIKEY_PROVIDERS,
		WEB_COOKIE_PROVIDERS: store.WEB_COOKIE_PROVIDERS,
		MEDIA_PROVIDER_KINDS: store.MEDIA_PROVIDER_KINDS,
		USAGE_SUPPORTED_PROVIDERS: store.USAGE_SUPPORTED_PROVIDERS,
		USAGE_APIKEY_PROVIDERS: store.USAGE_APIKEY_PROVIDERS,
		ALIAS_TO_ID,
		ID_TO_ALIAS,
		getProviderByAlias,
		resolveProviderId,
		getProviderAlias,
		getProvidersByKind,
	};
}
