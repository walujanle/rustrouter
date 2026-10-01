import { defineStore } from "pinia";

import { THEME_CONFIG } from "@/constants/config";

/**
 * Theme store. Persists to localStorage under the `theme` key in the
 * `{state:{theme}}` envelope — the pre-paint script in `index.html` reads that
 * exact shape, so the envelope must not change.
 */
export const useThemeStore = defineStore("theme", {
	state: () => ({
		theme: THEME_CONFIG.defaultTheme as "light" | "dark" | "system",
	}),
	actions: {
		applyTheme(theme: string) {
			if (typeof window === "undefined") return;
			const root = document.documentElement;
			const systemTheme = window.matchMedia("(prefers-color-scheme: dark)")
				.matches
				? "dark"
				: "light";
			const effectiveTheme = theme === "system" ? systemTheme : theme;
			if (effectiveTheme === "dark") root.classList.add("dark");
			else root.classList.remove("dark");
		},
		persist() {
			localStorage.setItem(
				THEME_CONFIG.storageKey,
				JSON.stringify({ state: { theme: this.theme } }),
			);
		},
		setTheme(theme: "light" | "dark" | "system") {
			this.theme = theme;
			this.applyTheme(theme);
			this.persist();
		},
		toggleTheme() {
			const next = this.theme === "dark" ? "light" : "dark";
			this.setTheme(next);
		},
		initTheme() {
			const raw = localStorage.getItem(THEME_CONFIG.storageKey);
			if (raw) {
				try {
					const parsed = JSON.parse(raw);
					if (parsed?.state?.theme) this.theme = parsed.state.theme;
				} catch {
					/* keep default */
				}
			}
			this.applyTheme(this.theme);
		},
	},
});
