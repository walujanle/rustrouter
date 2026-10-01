// Pure helpers for the combos page. The capability aggregator is computed over
// the browser-side caps map (`useModelCaps`), since the Vue app cannot import
// the engine.

// Validate combo name: only a-z, A-Z, 0-9, -, _
export const VALID_NAME_REGEX = /^[a-zA-Z0-9_.-]+$/;

export interface Combo {
	id: string;
	name: string;
	models: string[];
	kind?: string;
}

export interface CapEntry {
	enabled: boolean;
	roundRobin: boolean;
	models: string[];
}

export interface ComboCaps {
	vision: boolean;
	search: boolean;
	reasoning: boolean;
	contextWindow?: number;
	maxOutput?: number;
}

export interface ModelCaps {
	vision?: boolean;
	search?: boolean;
	reasoning?: boolean;
	contextWindow?: number;
	maxOutput?: number;
}

// Capacity adapter: global fallback pools of models per input-modality capability.
// A request needing a capability the target model/combo lacks switches straight
// to the first enabled model here instead of erroring or dropping the data.
export const CAPACITY_ADAPTER_CAPS = [
	{
		key: "vision",
		label: "Vision",
		icon: "visibility",
		desc: "images (png, jpg, webp, …)",
	},
	// pdf, videoInput temporarily hidden — no translator support yet for those blocks.
	{
		key: "audioInput",
		label: "Audio",
		icon: "graphic_eq",
		desc: "audio input",
	},
];

export const DEFAULT_FALLBACK_MODEL = "oc/mimo-v2.6-flash-free";
export const EMPTY_CAP_ENTRY: CapEntry = {
	enabled: true,
	roundRobin: false,
	models: [],
};
export const EMPTY_CAPACITY_ADAPTER: Record<string, CapEntry> = {
	vision: { ...EMPTY_CAP_ENTRY },
	pdf: { ...EMPTY_CAP_ENTRY },
	audioInput: { ...EMPTY_CAP_ENTRY },
	videoInput: { ...EMPTY_CAP_ENTRY },
};

const upgradeLegacyModel = (m: string) =>
	m === "oc/mimo-v2.5-free" ? DEFAULT_FALLBACK_MODEL : m;

// Backward-compat: legacy stored form was an array of {model, enabled}.
export function normalizeCapEntry(entry: unknown): CapEntry {
	if (Array.isArray(entry)) {
		return {
			enabled: true,
			roundRobin: false,
			models: entry
				.map((e) => upgradeLegacyModel((e as { model?: string })?.model || e))
				.filter(Boolean) as string[],
		};
	}
	if (entry && typeof entry === "object") {
		const e = entry as {
			enabled?: boolean;
			roundRobin?: boolean;
			models?: string[];
		};
		return {
			enabled: e.enabled !== false,
			roundRobin: !!e.roundRobin,
			models: Array.isArray(e.models)
				? e.models.map(upgradeLegacyModel).filter(Boolean)
				: [],
		};
	}
	return { ...EMPTY_CAP_ENTRY };
}

export const STRATEGY_OPTIONS = [
	{ value: "fallback", label: "Fallback — try in order" },
	{ value: "round-robin", label: "Round Robin — rotate" },
	{ value: "fusion", label: "Fusion — panel + judge" },
];

export const fmtK = (n?: number): string => {
	if (!n) return "?";
	if (n >= 1000000) {
		const m = n / 1000000;
		return `${Number.isInteger(m) ? m : m.toFixed(1)}M`;
	}
	return `${Math.round(n / 1000)}k`;
};

/**
 * Aggregate capabilities for a combo from its constituent model IDs.
 * Union for the capability flags (a combo supports a capability when any member
 * does); contextWindow is the smallest member value, maxOutput the largest.
 * Nested combos resolve through `comboByName` (bare name → member models).
 */
export function aggregateComboCapabilities(
	models: string[] | null | undefined,
	comboByName: Record<string, string[]> = {},
	getCaps?: (key: string) => ModelCaps | null,
	depth = 0,
): ComboCaps | null {
	if (!models?.length || depth > 6) return null;
	const all = models
		.map((m) => {
			// Nested combo: bare name (no slash) that exists in the lookup — recurse.
			if (!String(m).includes("/") && comboByName[m]) {
				return (
					aggregateComboCapabilities(
						comboByName[m],
						comboByName,
						getCaps,
						depth + 1,
					) ??
					getCaps?.(m) ??
					null
				);
			}
			return getCaps?.(m) ?? null;
		})
		.filter((c): c is ModelCaps => !!c);
	if (all.length === 0) return null;
	const ctxs = all
		.map((c) => c.contextWindow)
		.filter((n): n is number => typeof n === "number");
	const outs = all
		.map((c) => c.maxOutput)
		.filter((n): n is number => typeof n === "number");
	return {
		vision: all.some((c) => c.vision),
		search: all.some((c) => c.search),
		reasoning: all.some((c) => c.reasoning),
		contextWindow: ctxs.length ? Math.min(...ctxs) : undefined,
		maxOutput: outs.length ? Math.max(...outs) : undefined,
	};
}
