import { liveAppPort } from "@/constants/config";

// Browser-local preset stores (endpoints, API keys) shared by every CLI tool card
function createStore<T extends { name: string }>({
	storageKey,
	changeEvent,
	itemField,
	normalize = (v: string) => v,
	defaultName = (v: string) => v,
}: {
	storageKey: string;
	changeEvent: string;
	itemField: keyof T & string;
	normalize?: (v: string) => string;
	defaultName?: (v: string) => string;
}) {
	const read = (): T[] => {
		if (typeof window === "undefined") return [];
		try {
			const raw = JSON.parse(window.localStorage.getItem(storageKey) || "[]");
			if (!Array.isArray(raw)) return [];
			return raw.filter((p) => p?.name && p?.[itemField]) as T[];
		} catch {
			return [];
		}
	};

	const write = (items: T[]) => {
		if (typeof window === "undefined") return;
		window.localStorage.setItem(storageKey, JSON.stringify(items));
		window.dispatchEvent(new CustomEvent(changeEvent));
	};

	return {
		read,
		subscribe: (handler: () => void) => {
			if (typeof window === "undefined") return () => {};
			window.addEventListener(changeEvent, handler);
			return () => window.removeEventListener(changeEvent, handler);
		},
		// Adds or replaces a preset; returns the stored name, or null when skipped
		upsert: (value: string, name?: string): string | null => {
			const v = normalize(value);
			if (!v) return null;

			const items = read();
			const existing = items.find((p) => normalize(String(p[itemField])) === v);
			if (existing && !name) return existing.name;

			const finalName = (name || defaultName(v)).trim();
			if (!finalName) return null;

			const next = [
				...items.filter(
					(p) => p.name !== finalName && normalize(String(p[itemField])) !== v,
				),
				{ name: finalName, [itemField]: v } as unknown as T,
			].sort((a, b) => a.name.localeCompare(b.name));
			write(next);
			return finalName;
		},
		remove: (name: string) => write(read().filter((p) => p.name !== name)),
	};
}

const stripSlash = (url: string) => (url || "").replace(/\/+$/, "");

interface EndpointPreset {
	name: string;
	baseUrl: string;
}

interface ApiKeyPreset {
	name: string;
	key: string;
}

const endpoints = createStore<EndpointPreset>({
	storageKey: "9router.cliToolEndpointPresets",
	changeEvent: "9router:endpoint-presets-changed",
	itemField: "baseUrl",
	normalize: stripSlash,
	defaultName: (url) => {
		try {
			return new URL(url).host;
		} catch {
			return url;
		}
	},
});

const apiKeys = createStore<ApiKeyPreset>({
	storageKey: "9router.cliToolApiKeyPresets",
	changeEvent: "9router:api-key-presets-changed",
	itemField: "key",
});

export const readPresets = endpoints.read;
export const subscribePresets = endpoints.subscribe;
export const upsertPreset = endpoints.upsert;
export const deletePreset = endpoints.remove;

export const readKeyPresets = apiKeys.read;
export const subscribeKeyPresets = apiKeys.subscribe;
export const upsertKeyPreset = apiKeys.upsert;
export const deleteKeyPreset = apiKeys.remove;

// Save an applied endpoint unless it exactly matches a built-in dropdown option
export function rememberEndpoint(baseUrl: string): string | null {
	const url = stripSlash(baseUrl);
	if (!url) return null;

	const builtIns = [`http://127.0.0.1:${liveAppPort()}`].flatMap((u) => [
		stripSlash(u),
		`${stripSlash(u)}/v1`,
	]);
	if (builtIns.includes(url)) return null;

	return upsertPreset(url);
}

export { stripSlash };
