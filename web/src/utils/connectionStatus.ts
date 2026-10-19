/**
 * Connection-derived provider status for the pickers.
 *
 * `GET /api/providers` returns every connection, inactive ones included, so the
 * client has to decide what "deactivated" means. A provider is deactivated only
 * when it has at least one connection and every one of them is inactive.
 */
export interface ConnectionLike {
	provider?: string | null;
	isActive?: boolean | null;
}

export function getDeactivatedProviderIds(
	connections: ConnectionLike[],
): Set<string> {
	const seen = new Map<string, { total: number; active: number }>();
	for (const connection of connections) {
		const provider = connection.provider;
		if (typeof provider !== "string" || !provider) continue;
		const entry = seen.get(provider) ?? { total: 0, active: 0 };
		entry.total += 1;
		if (connection.isActive !== false) entry.active += 1;
		seen.set(provider, entry);
	}
	const out = new Set<string>();
	for (const [provider, entry] of seen) {
		if (entry.total > 0 && entry.active === 0) out.add(provider);
	}
	return out;
}
