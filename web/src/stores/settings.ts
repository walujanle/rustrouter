import { defineStore } from "pinia";

import { CLIENT_STORE_TTL_MS } from "@/constants/config";
import { get, patch } from "@/utils/api";

let inflight: Promise<Record<string, any> | null> | null = null;

/**
 * Server settings blob, cached for `CLIENT_STORE_TTL_MS` and coalesced across
 * concurrent callers. Consumers only ever read `settings` and call
 * `fetchSettings` / `patchSettings`.
 */
export const useSettingsStore = defineStore("settings", {
	state: () => ({
		settings: null as Record<string, any> | null,
		loading: false,
		error: null as string | null,
		lastFetched: 0,
	}),
	actions: {
		invalidate() {
			this.lastFetched = 0;
		},
		async fetchSettings({
			force = false,
		} = {}): Promise<Record<string, any> | null> {
			if (
				!force &&
				this.settings &&
				Date.now() - this.lastFetched < CLIENT_STORE_TTL_MS
			) {
				return this.settings;
			}
			if (inflight) return inflight;

			this.loading = true;
			this.error = null;
			inflight = (async () => {
				try {
					const data = (await get("/api/settings")) as Record<string, any>;
					this.settings = data;
					this.lastFetched = Date.now();
					return data;
				} catch (e) {
					this.error = (e as Error).message || "Failed to fetch settings";
					return null;
				} finally {
					this.loading = false;
					inflight = null;
				}
			})();
			return inflight;
		},
		async patchSettings(
			patchBody: Record<string, any>,
		): Promise<Record<string, any> | null> {
			try {
				const updated = (await patch("/api/settings", patchBody)) as Record<
					string,
					any
				>;
				// Merge, not replace: PATCH omits GET-only fields (e.g. hasPassword).
				this.settings = { ...this.settings, ...updated };
				this.lastFetched = Date.now();
				return updated;
			} catch {
				return null;
			}
		},
	},
});
