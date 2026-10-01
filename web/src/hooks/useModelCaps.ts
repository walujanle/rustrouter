import { onBeforeUnmount, onMounted, ref } from "vue";

import { get } from "@/utils/api";

export interface ModelCaps {
	vision: boolean;
	search: boolean;
	reasoning: boolean;
	contextWindow?: number;
	maxOutput?: number;
}

interface CapsMaps {
	byFull: Record<string, ModelCaps>;
	byId: Record<string, ModelCaps>;
}

// One /api/models fetch shared by every useModelCaps instance.
let cache: CapsMaps | null = null;
let inflight: Promise<CapsMaps> | null = null;

function buildMaps(models: Array<Record<string, any>>): CapsMaps {
	const byFull: Record<string, ModelCaps> = {};
	const byId: Record<string, ModelCaps> = {};
	for (const m of models || []) {
		if (!m.caps) continue;
		if (m.fullModel) byFull[m.fullModel] = m.caps;
		if (m.routedModel) byFull[m.routedModel] = m.caps;
		if (m.model) byId[m.model] = m.caps;
	}
	return { byFull, byId };
}

function loadModelCaps(): Promise<CapsMaps> {
	if (cache) return Promise.resolve(cache);
	if (inflight) return inflight;
	inflight = get("/api/models")
		.then((data: any) => {
			cache = buildMaps(data.models);
			return cache;
		})
		.catch(() => ({ byFull: {}, byId: {} }))
		.finally(() => {
			inflight = null;
		});
	return inflight;
}

/** Resolve caps from a `provider/model` string or a bare model id. */
export function useModelCaps() {
	const byFull = ref<Record<string, ModelCaps>>(cache?.byFull || {});
	const byId = ref<Record<string, ModelCaps>>(cache?.byId || {});

	let alive = true;
	const sync = (maps: CapsMaps) => {
		if (!alive) return;
		byFull.value = maps.byFull;
		byId.value = maps.byId;
	};

	// Custom models change at runtime — drop the shared cache and refetch.
	const invalidate = () => {
		cache = null;
		loadModelCaps().then(sync);
	};

	onMounted(() => {
		if (cache) sync(cache);
		else loadModelCaps().then(sync);
		window.addEventListener("customModelChanged", invalidate);
	});

	onBeforeUnmount(() => {
		alive = false;
		window.removeEventListener("customModelChanged", invalidate);
	});

	function getCaps(key: string | null | undefined): ModelCaps | null {
		if (!key) return null;
		const full = byFull.value[key];
		if (full) return full;
		const bare = key.includes("/") ? key.slice(key.indexOf("/") + 1) : key;
		if (byId.value[bare]) return byId.value[bare];
		return null;
	}

	return { getCaps };
}
