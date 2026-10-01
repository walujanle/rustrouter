import { defineStore } from "pinia";

import { get } from "@/utils/api";

/**
 * The static client contract, served once by `GET /api/registry` and installed
 * here at boot. The Vue app has no compile-time copy of these tables, so this
 * is the one place they live client-side. Every
 * consumer reads them synchronously off the store — the boot loader awaits
 * `load()` before the router mounts.
 *
 * See `docs/FRONTEND.md` §"Client-side constants coupling".
 */
export interface RegistryPayload {
	providers: Record<string, Record<string, any>>;
	mediaProviderKinds: Array<Record<string, any>>;
	usageSupportedProviders: string[];
	usageApikeyProviders: string[];
	providerModels: Record<string, Array<Record<string, any>>>;
	providerIdToAlias: Record<string, string>;
}

export const useRegistryStore = defineStore("registry", {
	state: () => ({
		payload: null as RegistryPayload | null,
		loading: false,
	}),
	getters: {
		loaded: (state) => state.payload !== null,
		/** `AI_PROVIDERS` — the five category maps merged in the order the router
		 * serves them. */
		AI_PROVIDERS(state): Record<string, Record<string, any>> {
			const p = state.payload?.providers;
			if (!p) return {};
			return {
				...(p.free || {}),
				...(p.freeTier || {}),
				...(p.oauth || {}),
				...(p.apikey || {}),
				...(p.webCookie || {}),
			};
		},
		FREE_PROVIDERS: (state) => state.payload?.providers.free || {},
		FREE_TIER_PROVIDERS: (state) => state.payload?.providers.freeTier || {},
		OAUTH_PROVIDERS: (state) => state.payload?.providers.oauth || {},
		APIKEY_PROVIDERS: (state) => state.payload?.providers.apikey || {},
		WEB_COOKIE_PROVIDERS: (state) => state.payload?.providers.webCookie || {},
		MEDIA_PROVIDER_KINDS: (state) => state.payload?.mediaProviderKinds || [],
		USAGE_SUPPORTED_PROVIDERS: (state) =>
			state.payload?.usageSupportedProviders || [],
		USAGE_APIKEY_PROVIDERS: (state) =>
			state.payload?.usageApikeyProviders || [],
		PROVIDER_MODELS: (state) => state.payload?.providerModels || {},
		PROVIDER_ID_TO_ALIAS: (state) => state.payload?.providerIdToAlias || {},
	},
	actions: {
		async load(): Promise<void> {
			if (this.payload || this.loading) return;
			this.loading = true;
			try {
				this.payload = (await get("/api/registry")) as RegistryPayload;
			} finally {
				this.loading = false;
			}
		},
	},
});
