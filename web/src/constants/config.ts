import pkg from "../../package.json";

// App configuration
export const APP_CONFIG = {
	name: "RustRouter",
	description: "AI Infrastructure Management",
	version: pkg.version,
};

// Updater configuration
export const UPDATER_CONFIG = {
	npmPackageName: "rustrouter",
	installCmd: "npm i -g rustrouter",
	installCmdLatest: "npm i -g rustrouter@latest",
	shutdownCountdownSec: 3,
	exitDelayMs: 500,
	statusPollIntervalMs: 1000,
	statusLogTailLines: 8,
	installRetries: 3,
	installRetryDelayMs: 5000,
	lingerAfterDoneMs: 30000,
	waitForExitMinMs: 5000,
	waitForExitMaxMs: 20000,
	waitForExitCheckMs: 500,
	appPort: 20129,
};

// The port the dashboard is actually served on. `appPort` is the build-time
// default, but the server can be started on another port (`rustrouter start
// --port N`), so anything that builds a local endpoint for the running server
// must read the live port from the page origin and only fall back to the
// default when there is no browser (SSR/build).
export function liveAppPort(): number {
	if (typeof window !== "undefined" && window.location.port) {
		const parsed = Number(window.location.port);
		if (Number.isInteger(parsed) && parsed > 0) return parsed;
	}
	return UPDATER_CONFIG.appPort;
}

// Theme configuration
export const THEME_CONFIG = {
	storageKey: "theme",
	defaultTheme: "system" as "light" | "dark" | "system",
};

// Subscription
export const SUBSCRIPTION_CONFIG = {
	price: 1.0,
	currency: "USD",
	interval: "month",
	planName: "Pro Plan",
};

// API endpoints
export const API_ENDPOINTS = {
	users: "/api/users",
	providers: "/api/providers",
	payments: "/api/payments",
	auth: "/api/auth",
};

export const CONSOLE_LOG_CONFIG = {
	maxLines: 200,
	pollIntervalMs: 1000,
};

// Client-side store TTL: how long fetched data stays fresh before re-fetching
export const CLIENT_STORE_TTL_MS = 60000;
