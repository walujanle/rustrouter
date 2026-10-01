// Fetch and cache suggested models for providers that expose a public models
// API. Fetches through the backend proxy to avoid CORS.

const CACHE_TTL_MS = 10 * 60 * 1000; // 10 minutes
const cache = new Map<
	string,
	{ data: Array<Record<string, any>>; expiresAt: number }
>();

export async function fetchSuggestedModels(
	fetcher: { url?: string; type?: string } | null | undefined,
): Promise<Array<Record<string, any>>> {
	if (!fetcher?.url || !fetcher?.type) return [];

	const cached = cache.get(fetcher.url);
	if (cached && Date.now() < cached.expiresAt) return cached.data;

	try {
		const params = new URLSearchParams({
			url: fetcher.url,
			type: fetcher.type,
		});
		const res = await fetch(`/api/providers/suggested-models?${params}`);
		if (!res.ok) return [];
		const json = await res.json();
		const data = json.data ?? [];
		cache.set(fetcher.url, { data, expiresAt: Date.now() + CACHE_TTL_MS });
		return data;
	} catch {
		return [];
	}
}
