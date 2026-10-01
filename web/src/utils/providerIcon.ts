// Provider icon paths under /public/providers.
// Session-cache 404s so one miss never spams again.

// Runtime only — first 404 remembers id for the whole session.
const failedIds = new Set<string>();

function normalizeId(providerId: unknown): string {
	if (!providerId || typeof providerId !== "string") return "";
	return providerId.trim().toLowerCase();
}

/** Resolve icon file id. Empty if previously failed this session. */
export function resolveProviderIconId(providerId: unknown): string {
	const id = normalizeId(providerId);
	if (!id || failedIds.has(id)) return "";
	return id;
}

/** `/providers/{id}.png` or null when previously failed. */
export function getProviderIconSrc(providerId: unknown): string | null {
	const id = resolveProviderIconId(providerId);
	return id ? `/providers/${id}.png` : null;
}

/** Call from img onError so later mounts skip the request. */
export function markProviderIconMissing(providerId: unknown): void {
	const id = normalizeId(providerId);
	if (id) failedIds.add(id);
}
