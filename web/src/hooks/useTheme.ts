import { computed, onBeforeUnmount, onMounted, ref } from "vue";

import { useThemeStore } from "@/stores/theme";

/** Theme with system-preference tracking. */
export function useTheme() {
	const store = useThemeStore();
	const systemPrefersDark = ref(false);

	let mql: MediaQueryList | null = null;
	const handleChange = () => {
		systemPrefersDark.value = mql?.matches ?? false;
		store.initTheme();
	};

	onMounted(() => {
		mql = window.matchMedia("(prefers-color-scheme: dark)");
		systemPrefersDark.value = mql.matches;
		store.initTheme();
		mql.addEventListener("change", handleChange);
	});

	onBeforeUnmount(() => mql?.removeEventListener("change", handleChange));

	const isDark = computed(
		() =>
			store.theme === "dark" ||
			(store.theme === "system" && systemPrefersDark.value),
	);

	return {
		theme: computed(() => store.theme),
		setTheme: (t: "light" | "dark" | "system") => store.setTheme(t),
		toggleTheme: () => store.toggleTheme(),
		isDark,
	};
}
