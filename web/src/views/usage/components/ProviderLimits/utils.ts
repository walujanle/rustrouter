import { useModels } from "@/constants/models";

// ─── Types ───────────────────────────────────────────────────────────────────

/** A normalized quota row. `remaining` is a 0-100 percentage for providers
 * that report one; providers with absolute credit counts set
 * `remainingPercentage` instead and let the bar compute from used/total. */
export interface QuotaRow {
	name: string;
	used: number;
	total: number;
	resetAt?: string | null;
	remaining?: number;
	remainingPercentage?: number;
	recurring?: boolean;
	unlimited?: boolean;
	isCreditBalance?: boolean;
	currency?: string;
	unit?: string;
	quotaType?: string;
	message?: string;
}

/** A quota row after `QuotaTable` normalizes it (index + derived remaining). */
export type NormalizedQuota = QuotaRow & { index: number; remaining: number };

export interface Connection {
	id: string;
	provider?: string;
	name?: string;
	email?: string;
	displayName?: string;
	authType?: string;
	isActive?: boolean;
	testStatus?: string;
	providerSpecificData?: Record<string, any>;
	[key: string]: any;
}

export interface QuotaEntry {
	quotas: QuotaRow[];
	plan?: string | null;
	message?: string | null;
	raw?: Record<string, any>;
}

export type QuotaData = Record<string, QuotaEntry>;

export interface Pagination {
	page: number;
	pageSize: number;
	total: number;
	totalPages: number;
}

export interface ConnectionTotals {
	eligibleConnections: number;
	providerFilteredConnections: number;
}

export interface EmptyState {
	icon: string;
	title: string;
	description: string;
}

// ─── Constants ───────────────────────────────────────────────────────────────
export const QUOTA_CACHE_KEY = "quotaCacheData";
export const REFRESH_INTERVAL_MS = 60000;
export const DEPLETED_QUOTA_THRESHOLD = 5;
export const AUTO_REFRESH_STORAGE_KEY = "quotaAutoRefresh";
export const CONNECTIONS_PAGE_SIZE = 20;
export const ACCOUNT_PAGE_SIZE_OPTIONS = [10, 20, 50, 100];
export const ACCOUNT_PAGE_SIZE_MAX = 500;
export const ACCOUNT_FILTER_OPTIONS = [
	{ value: "all", label: "All accounts" },
	{ value: "active", label: "Active" },
	{ value: "inactive", label: "Turned off" },
];
export const QUOTA_SORT_OPTIONS = [
	{ value: "default", label: "Default quota order" },
	{ value: "remaining-asc", label: "% quota: low to high" },
	{ value: "remaining-desc", label: "% quota: high to low" },
];

// ─── Pure helpers ─────────────────────────────────────────────────────────────
export function getConnectionLabel(connection: Connection): string | null {
	return (
		connection.name?.trim() ||
		connection.email?.trim() ||
		connection.displayName?.trim() ||
		null
	);
}

export function getConnectionQuotaRemaining(
	connection: Connection,
	quotaData: QuotaData,
): number {
	const quota = quotaData[connection.id]?.quotas?.[0];
	if (!quota) return Number.POSITIVE_INFINITY;
	if (typeof quota.remaining === "number") return quota.remaining;
	return Number.POSITIVE_INFINITY;
}

// Stable group-by-provider: first-seen provider order, original order within group.
function groupByProviderStable(connections: Connection[]): Connection[] {
	const seen = new Map<string, Connection[]>();
	for (const conn of connections) {
		const key = conn.provider || "";
		if (!seen.has(key)) seen.set(key, []);
		(seen.get(key) as Connection[]).push(conn);
	}
	return Array.from(seen.values()).flat();
}

export function sortVisibleConnections(
	connections: Connection[],
	quotaData: QuotaData,
	expiringFirst: boolean,
	providerFilter: string,
	quotaSortMode: string,
): Connection[] {
	if (providerFilter === "codex" && quotaSortMode !== "default") {
		return [...connections].sort((a, b) => {
			const remainingA = getConnectionQuotaRemaining(a, quotaData);
			const remainingB = getConnectionQuotaRemaining(b, quotaData);
			const remainingDiff =
				quotaSortMode === "remaining-asc"
					? remainingA - remainingB
					: remainingB - remainingA;
			if (remainingDiff !== 0) return remainingDiff;
			return (getConnectionLabel(a) || "").localeCompare(
				getConnectionLabel(b) || "",
			);
		});
	}

	if (!expiringFirst) return groupByProviderStable(connections);

	const getEarliestResetTime = (connection: Connection): number => {
		const resetTimes = (quotaData[connection.id]?.quotas || [])
			.map((quota) =>
				quota.resetAt
					? new Date(quota.resetAt).getTime()
					: Number.POSITIVE_INFINITY,
			)
			.filter((time) => Number.isFinite(time));
		return resetTimes.length > 0
			? Math.min(...resetTimes)
			: Number.POSITIVE_INFINITY;
	};

	return [...connections].sort((a, b) => {
		const expiryDiff = getEarliestResetTime(a) - getEarliestResetTime(b);
		if (expiryDiff !== 0) return expiryDiff;
		return (
			(a.provider || "").localeCompare(b.provider || "") ||
			(getConnectionLabel(a) || "").localeCompare(getConnectionLabel(b) || "")
		);
	});
}

export function buildLoadingState(
	connections: Connection[],
): Record<string, boolean> {
	const nextLoadingState: Record<string, boolean> = {};
	connections.forEach((connection) => {
		nextLoadingState[connection.id] = true;
	});
	return nextLoadingState;
}

export function filterQuotaStateByConnections<T>(
	state: Record<string, T>,
	connections: Connection[],
): Record<string, T> {
	const visibleIds = new Set(connections.map((connection) => connection.id));
	return Object.fromEntries(
		Object.entries(state).filter(([id]) => visibleIds.has(id)),
	);
}

export function getConnectionsPageRange(pagination: Pagination): {
	start: number;
	end: number;
} {
	if (!pagination.total) {
		return { start: 0, end: 0 };
	}
	const start = (pagination.page - 1) * pagination.pageSize + 1;
	const end = Math.min(pagination.page * pagination.pageSize, pagination.total);
	return { start, end };
}

export function getConnectionsEmptyMessage(
	totals: ConnectionTotals,
	providerFilter: string,
	accountFilter: string,
): EmptyState {
	if (!totals.eligibleConnections) {
		return {
			icon: "cloud_off",
			title: "No Providers Connected",
			description:
				"Connect to providers with OAuth to track your API quota limits and usage.",
		};
	}
	if (!totals.providerFilteredConnections) {
		return {
			icon: "filter_alt_off",
			title: "No Accounts Match Current Filters",
			description:
				providerFilter === "all"
					? "Try changing the account status filter to see more quota trackers."
					: `No ${accountFilter === "inactive" ? "turned off" : accountFilter === "active" ? "active" : "matching"} accounts found for ${providerFilter}.`,
		};
	}
	return {
		icon: "filter_alt_off",
		title: "No Accounts On This Page",
		description:
			"Try moving to another page or refreshing the current filters.",
	};
}

export function sortRequestFromExpiringFirst(expiringFirst: boolean): string {
	return expiringFirst ? "expiring" : "priority";
}

export function getPageSizeLabel(
	pageSize: number,
	isCustomPageSize: boolean,
): string {
	return isCustomPageSize ? `Custom: ${pageSize} / page` : `${pageSize} / page`;
}

export function getConnectionsPaginationSummary(
	pagination: Pagination,
): string {
	const { start, end } = getConnectionsPageRange(pagination);
	return `Showing ${start}-${end} of ${pagination.total}`;
}

export function getSafePagination(
	pagination: Pagination | null | undefined,
	fallbackPageSize: number,
): Pagination {
	return (
		pagination || {
			page: 1,
			pageSize: fallbackPageSize,
			total: 0,
			totalPages: 1,
		}
	);
}

export function getSafeTotals(
	totals: ConnectionTotals | null | undefined,
	fallbackTotal = 0,
): ConnectionTotals {
	return (
		totals || {
			eligibleConnections: fallbackTotal,
			providerFilteredConnections: fallbackTotal,
		}
	);
}

export function shouldResetPage(
	previousValue: string,
	nextValue: string,
): boolean {
	return previousValue !== nextValue;
}

export function getPaginationPageValue(
	dataPagination: Pagination | null | undefined,
	fallbackPage: number,
): number {
	return dataPagination?.page || fallbackPage;
}

export function getProviderOptions(
	dataProviderOptions: string[] | null | undefined,
): string[] {
	return dataProviderOptions || [];
}

export async function reconcileConnectionsPage(
	fetchConnections: (page: number) => Promise<Connection[]>,
	targetPage: number,
): Promise<Connection[]> {
	return await fetchConnections(targetPage);
}

export function getQuotaCache(): Record<string, any> {
	if (typeof window === "undefined") return {};
	try {
		const cached = window.localStorage.getItem(QUOTA_CACHE_KEY);
		return cached ? JSON.parse(cached) : {};
	} catch (error) {
		console.error("Error reading quota cache:", error);
		return {};
	}
}

export function setQuotaCache(
	connectionId: string,
	quotaEntry: Record<string, any>,
): void {
	if (typeof window === "undefined") return;
	try {
		const cache = getQuotaCache();
		cache[connectionId] = {
			...quotaEntry,
			cachedAt: new Date().toISOString(),
		};
		window.localStorage.setItem(QUOTA_CACHE_KEY, JSON.stringify(cache));
	} catch (error) {
		console.error("Error writing quota cache:", error);
	}
}

/**
 * Format ISO date string to countdown format
 * @returns Formatted countdown (e.g., "2d 5h 30m", "4h 40m", "15m") or "-"
 */
export function formatResetTime(
	date: string | Date | null | undefined,
): string {
	if (!date) return "-";

	try {
		const resetDate = typeof date === "string" ? new Date(date) : date;
		const now = new Date();
		const diffMs = resetDate.getTime() - now.getTime();

		if (diffMs <= 0) return "-";

		const totalMinutes = Math.ceil(diffMs / (1000 * 60));

		// < 60 minutes: show only minutes
		if (totalMinutes < 60) {
			return `${totalMinutes}m`;
		}

		const totalHours = Math.floor(totalMinutes / 60);
		const remainingMinutes = totalMinutes % 60;

		// < 24 hours: show hours and minutes
		if (totalHours < 24) {
			return `${totalHours}h ${remainingMinutes}m`;
		}

		// >= 24 hours: show days, hours, and minutes
		const days = Math.floor(totalHours / 24);
		const remainingHours = totalHours % 24;
		return `${days}d ${remainingHours}h ${remainingMinutes}m`;
	} catch {
		return "-";
	}
}

/** Remaining percentage from used/total. */
export function calculatePercentage(
	used: number | null | undefined,
	total: number | null | undefined,
): number {
	if (!total || total === 0) return 0;
	if (!used || used < 0) return 100;
	if (used >= total) return 0;

	return Math.round(((total - used) / total) * 100);
}

/** Remaining percentage from a normalized quota row. */
export function getRemainingPercentage(
	quota: Partial<QuotaRow> | null | undefined,
): number {
	if (quota?.remaining !== undefined) {
		return Math.max(0, Math.round(quota.remaining));
	}

	if (quota?.remainingPercentage !== undefined) {
		return Math.round(quota.remainingPercentage);
	}

	return calculatePercentage(quota?.used, quota?.total);
}

export function getQuotaVisibilityKey(
	quota: Partial<QuotaRow> | null | undefined,
): string {
	if (!quota || typeof quota !== "object") return "";
	return String(quota.name || "").trim();
}

/**
 * Trim hidden quota keys to only those matching currently valid quotas.
 * Stale or obsolete model keys are dropped.
 */
export function trimHiddenQuotaKeys(
	hidden: string[] = [],
	quotas: QuotaRow[] = [],
): string[] {
	if (!Array.isArray(hidden) || hidden.length === 0) return [];
	const validKeys = new Set(quotas.map(getQuotaVisibilityKey).filter(Boolean));
	return [
		...new Set(
			hidden.map((k) => String(k).trim()).filter((k) => validKeys.has(k)),
		),
	];
}

function getProviderHiddenQuotaSet(
	provider: string,
	quotaVisibility: Record<string, any>,
	quotas: QuotaRow[] = [],
): Set<string> {
	const hidden = quotaVisibility?.[provider]?.hidden;
	if (!Array.isArray(hidden) || hidden.length === 0) return new Set();
	const trimmed =
		quotas.length > 0 ? trimHiddenQuotaKeys(hidden, quotas) : hidden;
	return new Set(trimmed.map(String));
}

export function filterQuotasByVisibility(
	provider: string,
	quotas: QuotaRow[] = [],
	quotaVisibility: Record<string, any> = {},
): QuotaRow[] {
	if (!Array.isArray(quotas) || quotas.length === 0) return [];
	const hidden = getProviderHiddenQuotaSet(provider, quotaVisibility, quotas);
	if (hidden.size === 0) return quotas;
	return quotas.filter((quota) => !hidden.has(getQuotaVisibilityKey(quota)));
}

export function getHiddenQuotaRows(
	provider: string,
	quotas: QuotaRow[] = [],
	quotaVisibility: Record<string, any> = {},
): QuotaRow[] {
	if (!Array.isArray(quotas) || quotas.length === 0) return [];
	const hidden = getProviderHiddenQuotaSet(provider, quotaVisibility, quotas);
	if (hidden.size === 0) return [];
	return quotas.filter((quota) => hidden.has(getQuotaVisibilityKey(quota)));
}

/**
 * Parse provider-specific quota structures into normalized array.
 * @param provider - Provider name (codex, deepseek, grok-cli, …)
 * @param data - Raw quota data from provider
 * @returns Normalized quota objects with { name, used, total, resetAt }
 */
export function parseQuotaData(provider: string, data: any): QuotaRow[] {
	if (!data || typeof data !== "object") return [];

	const normalizedQuotas: QuotaRow[] = [];

	try {
		switch (provider.toLowerCase()) {
			case "codex":
				if (data.quotas) {
					Object.entries(data.quotas).forEach(
						([quotaType, quota]: [string, any]) => {
							let displayName = quotaType;
							if (quotaType === "spark_session") displayName = "Spark (5h)";
							else if (quotaType === "spark_weekly")
								displayName = "Spark (Weekly)";
							else if (quotaType === "session") displayName = "5h";
							else if (quotaType === "weekly") displayName = "Weekly";
							else if (quotaType === "review_session")
								displayName = "Review (5h)";
							else if (quotaType === "review_weekly")
								displayName = "Review (Weekly)";

							normalizedQuotas.push({
								name: displayName,
								quotaType,
								used: quota.used || 0,
								total: quota.total || 0,
								remaining: quota.remaining,
								resetAt: quota.resetAt || null,
							});
						},
					);
				}
				break;

			case "codebuddy-intl":
				// CodeBuddy mixes recurring refill packs ("Monthly"/"Weekly"/...)
				// with one-shot bonus packs ("Bonus Pack N"). Forward `recurring`
				// so the UI can show "Expires in" for bonus packs (whose resetAt is
				// a hard expiry, not a refresh) instead of "Reset in". Only the
				// `codebuddy-intl` entry exists here, so the id must match the
				// registry or every row falls to `default` and loses the flag.
				if (data.quotas) {
					Object.entries(data.quotas).forEach(
						([name, quota]: [string, any]) => {
							normalizedQuotas.push({
								name,
								used: quota.used || 0,
								total: quota.total || 0,
								resetAt: quota.resetAt || null,
								recurring: quota.recurring !== false,
							});
						},
					);
				}
				break;

			case "grok-cli":
				// Grok Build credits (on-demand window + prepaid balance).
				// Do NOT forward absolute `remaining` — getRemainingPercentage treats
				// it as a 0–100 percentage (same as Qoder). Use remainingPercentage.
				if (data.quotas) {
					Object.entries(data.quotas).forEach(
						([name, quota]: [string, any]) => {
							normalizedQuotas.push({
								name,
								used: quota.used || 0,
								total: quota.total || 0,
								resetAt: quota.resetAt || null,
								remainingPercentage: quota.remainingPercentage,
							});
						},
					);
				}
				break;

			case "deepseek":
				// Credit balance — remainingPercentage only (no absolute remaining).
				if (data.quotas) {
					Object.entries(data.quotas).forEach(
						([name, quota]: [string, any]) => {
							normalizedQuotas.push({
								name,
								used: quota.used || 0,
								total: quota.total || 0,
								resetAt: quota.resetAt || null,
								remainingPercentage: quota.remainingPercentage,
								isCreditBalance: quota.isCreditBalance ?? true,
								currency:
									quota.currency ||
									(name.includes("(")
										? name.slice(name.indexOf("(") + 1, name.indexOf(")"))
										: "USD"),
							});
						},
					);
				}
				break;

			default:
				// Generic fallback for unknown providers
				if (data.quotas) {
					Object.entries(data.quotas).forEach(
						([name, quota]: [string, any]) => {
							normalizedQuotas.push({
								name,
								used: quota.used || 0,
								total: quota.total || 0,
								resetAt: quota.resetAt || null,
							});
						},
					);
				}
		}
	} catch (error) {
		console.error(`Error parsing quota data for ${provider}:`, error);
		return [];
	}

	// Sort quotas according to PROVIDER_MODELS order
	const { getModelsByProviderId } = useModels();
	const modelOrder = getModelsByProviderId(provider);
	if (modelOrder.length > 0) {
		const orderMap = new Map(modelOrder.map((m, i) => [m.id, i]));

		normalizedQuotas.sort((a, b) => {
			const orderA = orderMap.get(a.name) ?? 999;
			const orderB = orderMap.get(b.name) ?? 999;
			return orderA - orderB;
		});
	}

	return normalizedQuotas;
}
