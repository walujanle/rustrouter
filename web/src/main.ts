import "material-symbols/outlined.css";
import "./style.css";

import { createPinia } from "pinia";
import { createApp } from "vue";

import App from "./App.vue";
import { router } from "./router";
import { useRegistryStore } from "./stores/registry";
import { useThemeStore } from "./stores/theme";

const pinia = createPinia();

async function bootstrap() {
	// The static client contract (providers, models, aliases) has to be present
	// before any route mounts, because the constants modules read it
	// synchronously. A failure leaves the store empty rather than blocking boot;
	// the pages surface the empty state.
	try {
		await useRegistryStore(pinia).load();
	} catch {
		/* the registry store keeps its empty default */
	}

	useThemeStore(pinia).initTheme();

	const app = createApp(App);
	app.use(pinia);
	app.use(router);
	app.mount("#app");
}

bootstrap();
