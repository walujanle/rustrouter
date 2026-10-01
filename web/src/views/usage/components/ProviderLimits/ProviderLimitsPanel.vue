<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import EditConnectionModal from "@/components/EditConnectionModal.vue";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Card from "@/components/ui/UiCard.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import Tooltip from "@/components/ui/UiTooltip.vue";
import { useProviders } from "@/constants/providers";
import { useSettingsStore } from "@/stores/settings";
import QuotaTable from "./QuotaTable.vue";
import type { Connection, NormalizedQuota, Pagination, QuotaRow } from "./utils";
import {
	ACCOUNT_FILTER_OPTIONS,
	ACCOUNT_PAGE_SIZE_MAX,
	ACCOUNT_PAGE_SIZE_OPTIONS,
	AUTO_REFRESH_STORAGE_KEY,
	buildLoadingState,
	CONNECTIONS_PAGE_SIZE,
	calculatePercentage,
	DEPLETED_QUOTA_THRESHOLD,
	filterQuotaStateByConnections,
	filterQuotasByVisibility,
	getConnectionLabel,
	getConnectionsEmptyMessage,
	getConnectionsPaginationSummary,
	getHiddenQuotaRows,
	getPaginationPageValue,
	getProviderOptions,
	getQuotaCache,
	getQuotaVisibilityKey,
	getSafePagination,
	getSafeTotals,
	parseQuotaData,
	QUOTA_CACHE_KEY,
	QUOTA_SORT_OPTIONS,
	REFRESH_INTERVAL_MS,
	reconcileConnectionsPage,
	setQuotaCache,
	shouldResetPage,
	sortVisibleConnections,
} from "./utils";

const { AI_PROVIDERS, USAGE_SUPPORTED_PROVIDERS } = useProviders();
const settingsStore = useSettingsStore();

const AUTO_PING_SETTINGS_KEYS: Record<string, string> = {
	codex: "codexAutoPing",
};

const AUTO_PING_TOOLTIPS: Record<string, string> = {
	codex:
		"Auto-starts the next 5h Codex window after reset by sending a tiny gpt-5.5 request. Consumes a small amount of quota.",
};

function getConnectionSecondaryLabel(connection: Connection) {
	if (
		connection.name?.trim() &&
		connection.email?.trim() &&
		connection.name.trim() !== connection.email.trim()
	) {
		return connection.email.trim();
	}

	if (
		connection.name?.trim() &&
		connection.displayName?.trim() &&
		connection.name.trim() !== connection.displayName.trim()
	) {
		return connection.displayName.trim();
	}

	return null;
}

function getCodexResetCreditCount(quota: any) {
	const value = quota?.raw?.resetCredits?.availableCount;
	const count = typeof value === "number" ? value : Number(value);
	return Number.isFinite(count) ? Math.max(0, count) : 0;
}

function providerLabel(providerId: string) {
	return AI_PROVIDERS[providerId]?.name || providerId;
}

function formatCreditDate(value: string | null | undefined) {
	if (!value) return "N/A";
	const date = new Date(value);
	if (!Number.isFinite(date.getTime())) return "N/A";
	return date.toLocaleString("en-US", {
		month: "short",
		day: "numeric",
		year: "numeric",
		hour: "numeric",
		minute: "2-digit",
	});
}

function formatTimeRemaining(value: string | null | undefined) {
	if (!value) return "N/A";
	const diffMs = new Date(value).getTime() - Date.now();
	if (!Number.isFinite(diffMs)) return "N/A";
	if (diffMs <= 0) return "Expired";
	const totalHours = Math.ceil(diffMs / (60 * 60 * 1000));
	const days = Math.floor(totalHours / 24);
	const hours = totalHours % 24;
	return days > 0 ? `${days}d ${hours}h` : `${hours}h`;
}

const connections = ref<Connection[]>([]);
const quotaData = ref<Record<string, any>>({});
const loading = ref<Record<string, boolean>>({});
const errors = ref<Record<string, string | null>>({});
const autoRefresh = ref(true);
const autoPingMaps = ref<{ codex: Record<string, boolean> }>({
	codex: {},
});
const hasHydratedAutoRefresh = ref(false);
const refreshingAll = ref(false);
const countdown = ref(60);
const connectionsLoading = ref(true);
const deletingId = ref<string | null>(null);
const togglingId = ref<string | null>(null);
const resettingLimitId = ref<string | null>(null);
const resetConfirmState = ref<{ connection: Connection; resetCreditCount: number } | null>(null);
const resetCreditsState = ref<any>(null);
const showEditModal = ref(false);
const selectedConnection = ref<Connection | null>(null);
const proxyPools = ref<Array<{ id: string; name: string }>>([]);
const providerFilter = ref("all");
const providerOptions = ref<string[]>([]);
const accountFilter = ref("all");
const quotaSortMode = ref("default");
const quotaVisibility = ref<Record<string, any>>({});
const expiringFirst = ref(false);
const providerMenuOpen = ref(false);
const bulkToggling = ref(false);
const page = ref(1);
const pageSize = ref(CONNECTIONS_PAGE_SIZE);
const customPageSizeInput = ref(String(CONNECTIONS_PAGE_SIZE));
const pagination = ref<Pagination>({
	page: 1,
	pageSize: CONNECTIONS_PAGE_SIZE,
	total: 0,
	totalPages: 1,
});
const totals = ref({ eligibleConnections: 0, providerFilteredConnections: 0 });
// Auto quota tracker (server-side): default off.
const quotaAutoTrackerEnabled = ref(false);

let intervalRef: ReturnType<typeof setInterval> | null = null;
let countdownRef: ReturnType<typeof setInterval> | null = null;

async function fetchConnections(targetPage = page.value) {
	try {
		const params = new URLSearchParams({
			page: String(targetPage),
			pageSize: String(pageSize.value),
			accountStatus: accountFilter.value,
			sort: "priority",
		});

		if (providerFilter.value !== "all") {
			params.set("provider", providerFilter.value);
		}

		const response = await fetch(`/api/providers/client?${params.toString()}`);
		if (!response.ok) throw new Error("Failed to fetch connections");

		const data = await response.json();
		const connectionList = data.connections || [];
		const nextPagination = getSafePagination(data.pagination, pageSize.value);
		const nextTotals = getSafeTotals(data.totals, connectionList.length);

		connections.value = connectionList;
		providerOptions.value = getProviderOptions(data.providerOptions);
		pagination.value = nextPagination;
		totals.value = nextTotals;
		page.value = getPaginationPageValue(data.pagination, targetPage);
		return connectionList as Connection[];
	} catch (error) {
		console.error("Error fetching connections:", error);
		connections.value = [];
		providerOptions.value = [];
		pagination.value = { page: 1, pageSize: pageSize.value, total: 0, totalPages: 1 };
		totals.value = { eligibleConnections: 0, providerFilteredConnections: 0 };
		return [];
	}
}

// Fetch quota for a specific connection
async function fetchQuota(
	connectionId: string,
	provider: string,
	{ force = false }: { force?: boolean } = {},
) {
	loading.value = { ...loading.value, [connectionId]: true };
	errors.value = { ...errors.value, [connectionId]: null };

	try {
		console.log(`[ProviderLimits] Fetching quota for ${provider} (${connectionId})`);
		const url = `/api/usage/${connectionId}${force ? "?force=1" : ""}`;
		const response = await fetch(url);

		if (!response.ok) {
			const errorData = await response.json().catch(() => ({}));
			const errorMsg = errorData.error || response.statusText;

			// Handle different error types gracefully
			if (response.status === 404) {
				console.warn(`[ProviderLimits] Connection not found for ${provider}, skipping`);
				return;
			}

			if (response.status === 401) {
				console.warn(`[ProviderLimits] Auth error for ${provider}:`, errorMsg);
				const quotaEntry = { quotas: [], message: errorMsg };
				quotaData.value = { ...quotaData.value, [connectionId]: quotaEntry };
				setQuotaCache(connectionId, quotaEntry);
				return;
			}

			throw new Error(`HTTP ${response.status}: ${errorMsg}`);
		}

		const data = await response.json();
		console.log(`[ProviderLimits] Got quota for ${provider}:`, data);

		const parsedQuotas = parseQuotaData(provider, data);

		const quotaEntry = {
			quotas: parsedQuotas,
			plan: data.plan || null,
			message: data.message || null,
			raw: data,
		};

		quotaData.value = { ...quotaData.value, [connectionId]: quotaEntry };
		setQuotaCache(connectionId, quotaEntry);
	} catch (error) {
		console.error(
			`[ProviderLimits] Error fetching quota for ${provider} (${connectionId}):`,
			error,
		);
		errors.value = {
			...errors.value,
			[connectionId]: (error as Error).message || "Failed to fetch quota",
		};
	} finally {
		loading.value = { ...loading.value, [connectionId]: false };
	}
}

// Refresh quota for a specific provider
async function refreshProvider(connectionId: string, provider: string) {
	await fetchQuota(connectionId, provider, { force: true });
}

async function handleResetCodexLimit(connectionId: string, provider: string) {
	if (provider !== "codex" || resettingLimitId.value) return;

	resettingLimitId.value = connectionId;
	errors.value = { ...errors.value, [connectionId]: null };

	try {
		const response = await fetch(`/api/usage/${connectionId}/codex-reset-credits`, {
			method: "POST",
		});
		const result = await response.json().catch(() => ({}));

		if (!response.ok) {
			throw new Error(result.message || result.error || result.code || "Failed to reset limit");
		}

		await fetchQuota(connectionId, provider, { force: true });
	} catch (error) {
		errors.value = {
			...errors.value,
			[connectionId]: (error as Error).message || "Failed to reset limit",
		};
	} finally {
		resettingLimitId.value = null;
	}
}

async function handleViewCodexResetCredits(connection: Connection) {
	resetCreditsState.value = { connection, loading: true, error: null, data: null };
	try {
		const response = await fetch(`/api/usage/${connection.id}/codex-reset-credits`, {
			cache: "no-store",
		});
		const result = await response.json().catch(() => ({}));
		if (!response.ok) {
			throw new Error(result.error || result.message || "Failed to load Codex reset credits");
		}
		const credits = Array.isArray(result.credits) ? [...result.credits] : [];
		credits.sort((a: any, b: any) => {
			const aTime = a.expiresAt ? new Date(a.expiresAt).getTime() : Number.POSITIVE_INFINITY;
			const bTime = b.expiresAt ? new Date(b.expiresAt).getTime() : Number.POSITIVE_INFINITY;
			return aTime - bTime;
		});
		resetCreditsState.value = {
			connection,
			loading: false,
			error: null,
			data: { ...result, credits },
		};
	} catch (error) {
		resetCreditsState.value = {
			connection,
			loading: false,
			error: (error as Error).message || "Failed to load Codex reset credits",
			data: null,
		};
	}
}

async function handleDeleteConnection(id: string) {
	if (!confirm("Delete this connection?")) return;
	deletingId.value = id;
	try {
		const res = await fetch(`/api/providers/${id}`, { method: "DELETE" });
		if (res.ok) {
			const nextQuota = { ...quotaData.value };
			delete nextQuota[id];
			quotaData.value = nextQuota;

			const nextLoading = { ...loading.value };
			delete nextLoading[id];
			loading.value = nextLoading;

			const nextErrors = { ...errors.value };
			delete nextErrors[id];
			errors.value = nextErrors;

			try {
				const cache = getQuotaCache();
				if (cache[id]) {
					delete cache[id];
					window.localStorage.setItem(QUOTA_CACHE_KEY, JSON.stringify(cache));
				}
			} catch (e) {
				console.error("Error deleting cache entry:", e);
			}

			await reconcileConnectionsPage(fetchConnections, page.value);
		}
	} catch (error) {
		console.error("Error deleting connection:", error);
	} finally {
		deletingId.value = null;
	}
}

async function handleToggleConnectionActive(id: string, isActive: boolean) {
	togglingId.value = id;
	try {
		const res = await fetch(`/api/providers/${id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ isActive }),
		});
		if (res.ok) {
			quotaData.value = { ...quotaData.value };
			await reconcileConnectionsPage(fetchConnections, page.value);
		}
	} catch (error) {
		console.error("Error updating connection status:", error);
	} finally {
		togglingId.value = null;
	}
}

async function handleUpdateConnection(formData: Record<string, any>) {
	if (!selectedConnection.value?.id) return;
	const connectionId = selectedConnection.value.id;
	const provider = selectedConnection.value.provider;
	try {
		const res = await fetch(`/api/providers/${connectionId}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(formData),
		});
		if (res.ok) {
			await fetchConnections();
			showEditModal.value = false;
			selectedConnection.value = null;
			if (provider && USAGE_SUPPORTED_PROVIDERS.includes(provider)) {
				await fetchQuota(connectionId, provider);
			}
		}
	} catch (error) {
		console.error("Error saving connection:", error);
	}
}

async function refreshAll() {
	if (refreshingAll.value) return;

	refreshingAll.value = true;
	countdown.value = 60;

	try {
		const visibleConnections = await fetchConnections(page.value);

		loading.value = buildLoadingState(visibleConnections);
		errors.value = filterQuotaStateByConnections(errors.value, visibleConnections);
		quotaData.value = filterQuotaStateByConnections(quotaData.value, visibleConnections);

		await Promise.all(
			visibleConnections.map((conn) => fetchQuota(conn.id, conn.provider ?? "")),
		);
	} catch (error) {
		console.error("Error refreshing all providers:", error);
	} finally {
		refreshingAll.value = false;
	}
}

async function initializeData() {
	connectionsLoading.value = true;
	const visibleConnections = await fetchConnections(page.value);
	connectionsLoading.value = false;

	// Always fetch fresh quota on mount, no cache display
	loading.value = buildLoadingState(visibleConnections);
	errors.value = filterQuotaStateByConnections(errors.value, visibleConnections);
	quotaData.value = filterQuotaStateByConnections(quotaData.value, visibleConnections);

	await Promise.all(visibleConnections.map((conn) => fetchQuota(conn.id, conn.provider ?? "")));
}

async function toggleAutoPing(connectionId: string, provider: string, on: boolean) {
	const settingsKey = AUTO_PING_SETTINGS_KEYS[provider];
	if (!settingsKey) return;

	const previous = autoPingMaps.value;
	const nextProviderMap = { ...(autoPingMaps.value[provider as "codex"] || {}), [connectionId]: on };
	const nextMaps = { ...autoPingMaps.value, [provider]: nextProviderMap };
	autoPingMaps.value = nextMaps as typeof autoPingMaps.value;
	try {
		const r = await fetch("/api/settings", { cache: "no-store" });
		const s = r.ok ? await r.json() : {};
		const cfg = { ...(s[settingsKey] || {}), connections: nextProviderMap };
		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ [settingsKey]: cfg }),
		});
	} catch {
		autoPingMaps.value = previous;
	}
}

async function toggleAutoTracker() {
	const next = !quotaAutoTrackerEnabled.value;
	quotaAutoTrackerEnabled.value = next;
	const updated = await settingsStore.patchSettings({ quotaAutoTrackerEnabled: next });
	if (!updated) quotaAutoTrackerEnabled.value = !next;
}

async function updateQuotaVisibility(nextVisibility: Record<string, any>, previousVisibility: Record<string, any>) {
	quotaVisibility.value = nextVisibility;
	try {
		const response = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ quotaVisibility: nextVisibility }),
		});
		if (!response.ok) throw new Error("Failed to update quota visibility");
	} catch (error) {
		console.error("Error updating quota visibility:", error);
		quotaVisibility.value = previousVisibility;
	}
}

function handleHideQuota(provider: string, quota: QuotaRow) {
	const key = getQuotaVisibilityKey(quota);
	if (!provider || !key) return;

	const previous = quotaVisibility.value;
	const providerVisibility = previous[provider] || {};
	const hidden = new Set<string>(providerVisibility.hidden || []);
	hidden.add(key);
	const next = {
		...previous,
		[provider]: { ...providerVisibility, hidden: [...hidden] },
	};
	updateQuotaVisibility(next, previous);
}

function handleShowQuota(provider: string, quota: QuotaRow) {
	const key = getQuotaVisibilityKey(quota);
	if (!provider || !key) return;

	const previous = quotaVisibility.value;
	const providerVisibility = previous[provider] || {};
	const hidden = new Set<string>(providerVisibility.hidden || []);
	hidden.delete(key);
	const next = {
		...previous,
		[provider]: { ...providerVisibility, hidden: [...hidden] },
	};
	updateQuotaVisibility(next, previous);
}

const sortedConnections = computed(() =>
	sortVisibleConnections(
		connections.value,
		quotaData.value,
		expiringFirst.value,
		providerFilter.value,
		quotaSortMode.value,
	),
);

// Connection is depleted when any quota entry hit the threshold
function isConnectionDepleted(conn: Connection) {
	const quotas = quotaData.value[conn.id]?.quotas;
	if (!quotas?.length) return false;
	return quotas.some((q: QuotaRow) => {
		if (!q.total || q.total <= 0) return false;
		return calculatePercentage(q.used, q.total) <= DEPLETED_QUOTA_THRESHOLD;
	});
}

async function bulkSetActive(targetIds: string[], isActive: boolean) {
	if (!targetIds.length || bulkToggling.value) return;
	bulkToggling.value = true;
	try {
		await Promise.all(
			targetIds.map((id) =>
				fetch(`/api/providers/${id}`, {
					method: "PUT",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify({ isActive }),
				}),
			),
		);
		await reconcileConnectionsPage(fetchConnections, page.value);
	} catch (error) {
		console.error("Error bulk toggling connections:", error);
	} finally {
		bulkToggling.value = false;
	}
}

function handleDisableDepleted() {
	const ids = sortedConnections.value
		.filter((c) => (c.isActive ?? true) && isConnectionDepleted(c))
		.map((c) => c.id);
	bulkSetActive(ids, false);
}

function handleEnableAvailable() {
	const ids = sortedConnections.value
		.filter((c) => !(c.isActive ?? true) && !isConnectionDepleted(c))
		.map((c) => c.id);
	bulkSetActive(ids, true);
}

const selectedProviderLabel = computed(() =>
	providerFilter.value === "all" ? "All providers" : providerLabel(providerFilter.value),
);
const hasEligibleConnections = computed(() => totals.value.eligibleConnections > 0);
const hasVisibleConnections = computed(() => sortedConnections.value.length > 0);
const emptyState = computed(() =>
	getConnectionsEmptyMessage(totals.value, providerFilter.value, accountFilter.value),
);
const connectionsPageSummary = computed(() => getConnectionsPaginationSummary(pagination.value));
const isCustomPageSize = computed(() => !ACCOUNT_PAGE_SIZE_OPTIONS.includes(pageSize.value));

function applyCustomPageSize() {
	const parsedValue = Number.parseInt(customPageSizeInput.value, 10);
	if (!Number.isFinite(parsedValue)) {
		customPageSizeInput.value = String(pageSize.value);
		return;
	}
	const nextPageSize = Math.min(ACCOUNT_PAGE_SIZE_MAX, Math.max(1, parsedValue));
	page.value = 1;
	pageSize.value = nextPageSize;
	customPageSizeInput.value = String(nextPageSize);
}

function handlePageSizeSelect(event: Event) {
	const nextValue = (event.target as HTMLSelectElement).value;
	if (nextValue === "custom") return;
	const nextPageSize = Number.parseInt(nextValue, 10);
	if (Number.isFinite(nextPageSize)) {
		page.value = 1;
		pageSize.value = nextPageSize;
		customPageSizeInput.value = String(nextPageSize);
	}
}

function handleAccountFilterChange(event: Event) {
	const nextValue = (event.target as HTMLSelectElement).value;
	if (shouldResetPage(accountFilter.value, nextValue)) page.value = 1;
	accountFilter.value = nextValue;
}

function selectProvider(value: string) {
	if (shouldResetPage(providerFilter.value, value)) page.value = 1;
	providerFilter.value = value;
	providerMenuOpen.value = false;
}

function handleVisibilityChange() {
	if (document.hidden) {
		if (intervalRef) {
			clearInterval(intervalRef);
			intervalRef = null;
		}
		if (countdownRef) {
			clearInterval(countdownRef);
			countdownRef = null;
		}
	} else if (autoRefresh.value && hasHydratedAutoRefresh.value) {
		intervalRef = setInterval(() => refreshAll(), REFRESH_INTERVAL_MS);
		countdownRef = setInterval(() => {
			countdown.value = countdown.value <= 1 ? 60 : countdown.value - 1;
		}, 1000);
	}
}

function startAutoRefresh() {
	stopAutoRefresh();
	intervalRef = setInterval(() => refreshAll(), REFRESH_INTERVAL_MS);
	countdownRef = setInterval(() => {
		countdown.value = countdown.value <= 1 ? 60 : countdown.value - 1;
	}, 1000);
}

function stopAutoRefresh() {
	if (intervalRef) {
		clearInterval(intervalRef);
		intervalRef = null;
	}
	if (countdownRef) {
		clearInterval(countdownRef);
		countdownRef = null;
	}
}

watch(
	[autoRefresh, hasHydratedAutoRefresh],
	([enabled, hydrated]) => {
		if (!hydrated || !enabled) stopAutoRefresh();
		else startAutoRefresh();
	},
);

// Re-init when the pagination or filter inputs change.
watch([page, accountFilter, pageSize, providerFilter], () => {
	initializeData();
});

onMounted(async () => {
	const stored = window.localStorage.getItem(AUTO_REFRESH_STORAGE_KEY);
	autoRefresh.value = stored === null ? true : stored === "true";
	hasHydratedAutoRefresh.value = true;

	await initializeData();

	fetch("/api/proxy-pools?isActive=true", { cache: "no-store" })
		.then((res) => res.json())
		.then((data) => {
			if (data?.proxyPools) proxyPools.value = data.proxyPools;
		})
		.catch(() => {});

	fetch("/api/settings", { cache: "no-store" })
		.then((r) => (r.ok ? r.json() : {}))
		.then((s: any) => {
			autoPingMaps.value = {
				codex: s?.codexAutoPing?.connections || {},
			};
			quotaVisibility.value = s?.quotaVisibility || {};
			quotaAutoTrackerEnabled.value = s?.quotaAutoTrackerEnabled === true;
		})
		.catch(() => {});

	document.addEventListener("visibilitychange", handleVisibilityChange);
});

// Persist auto-refresh preference
watch([autoRefresh, hasHydratedAutoRefresh], () => {
	if (!hasHydratedAutoRefresh.value) return;
	window.localStorage.setItem(AUTO_REFRESH_STORAGE_KEY, String(autoRefresh.value));
});

onBeforeUnmount(() => {
	stopAutoRefresh();
	document.removeEventListener("visibilitychange", handleVisibilityChange);
});
</script>

<template>
  <!-- No eligible connections at all -->
  <Card v-if="!connectionsLoading && !hasEligibleConnections" padding="lg">
    <div class="text-center py-12">
      <span class="material-symbols-outlined text-[64px] text-text-muted opacity-20"> cloud_off </span>
      <h3 class="mt-4 text-lg font-semibold text-text-primary">No Providers Connected</h3>
      <p class="mt-2 text-sm text-text-muted max-w-md mx-auto">
        Connect to providers with OAuth to track your API quota limits and usage.
      </p>
    </div>
  </Card>

  <!-- Eligible but nothing on this page/filter -->
  <Card v-else-if="!connectionsLoading && !hasVisibleConnections" padding="lg">
    <div class="text-center py-12">
      <span class="material-symbols-outlined text-[64px] text-text-muted opacity-20">
        {{ emptyState.icon }}
      </span>
      <h3 class="mt-4 text-lg font-semibold text-text-primary">{{ emptyState.title }}</h3>
      <p class="mt-2 text-sm text-text-muted max-w-md mx-auto">{{ emptyState.description }}</p>
    </div>
  </Card>

  <div v-else class="space-y-6">
    <!-- Header Controls -->
    <div class="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-end">
      <div class="flex flex-wrap items-center gap-1.5">
        <div class="relative">
          <button
            type="button"
            class="flex h-8 items-center justify-between gap-1 rounded-lg border border-black/10 bg-black/2 px-2 text-xs text-text-primary transition-colors hover:bg-black/5 dark:border-white/10 dark:bg-white/3 dark:hover:bg-white/10"
            aria-haspopup="menu"
            :aria-expanded="providerMenuOpen"
            title="Filter quota providers"
            @click="providerMenuOpen = !providerMenuOpen"
          >
            <span class="flex min-w-0 items-center gap-1.5">
              <span
                v-if="providerFilter === 'all'"
                class="material-symbols-outlined text-[14px] text-text-muted"
              >
                apps
              </span>
              <ProviderIcon
                v-else
                :src="`/providers/${providerFilter}.png`"
                :alt="providerFilter"
                :size="18"
                class-name="size-4.5 rounded object-contain"
                :fallback-text="providerFilter.slice(0, 2).toUpperCase()"
              />
              <span class="truncate hidden lg:inline">{{ selectedProviderLabel }}</span>
            </span>
            <span class="material-symbols-outlined text-[14px] text-text-muted"> expand_more </span>
          </button>

          <template v-if="providerMenuOpen">
            <button
              type="button"
              class="fixed inset-0 z-30 bg-transparent"
              aria-label="Close provider filter"
              @click="providerMenuOpen = false"
            />
            <div
              class="absolute left-0 z-40 mt-2 w-64 overflow-hidden rounded-2xl border border-black/10 bg-surface/95 p-1.5 shadow-xl shadow-black/10 backdrop-blur dark:border-white/10 dark:bg-surface/95 sm:w-72"
            >
              <button
                type="button"
                :class="`flex w-full items-center gap-3 rounded-xl px-3 py-2.5 text-left text-sm transition-colors ${providerFilter === 'all' ? 'bg-primary/10 text-primary' : 'text-text-primary hover:bg-black/5 dark:hover:bg-white/10'}`"
                @click="selectProvider('all')"
              >
                <span class="material-symbols-outlined text-[22px]"> apps </span>
                <span class="font-medium">All providers</span>
                <span
                  v-if="providerFilter === 'all'"
                  class="material-symbols-outlined ml-auto text-[20px]"
                >
                  check
                </span>
              </button>
              <div class="my-1 h-px bg-black/10 dark:bg-white/10" />
              <div class="max-h-72 overflow-y-auto pr-1">
                <button
                  v-for="provider in providerOptions"
                  :key="provider"
                  type="button"
                  :class="`flex w-full items-center gap-3 rounded-xl px-3 py-2.5 text-left text-sm transition-colors ${providerFilter === provider ? 'bg-primary/10 text-primary' : 'text-text-primary hover:bg-black/5 dark:hover:bg-white/10'}`"
                  @click="selectProvider(provider)"
                >
                  <ProviderIcon
                    :src="`/providers/${provider}.png`"
                    :alt="provider"
                    :size="24"
                    class-name="size-6 rounded-md object-contain"
                    :fallback-text="provider.slice(0, 2).toUpperCase()"
                  />
                  <span class="font-medium">{{ providerLabel(provider) }}</span>
                  <span
                    v-if="providerFilter === provider"
                    class="material-symbols-outlined ml-auto text-[20px]"
                  >
                    check
                  </span>
                </button>
              </div>
            </div>
          </template>
        </div>

        <select
          :value="accountFilter"
          class="h-8 rounded-lg border border-black/10 bg-black/2 px-2 text-xs text-text-primary outline-none transition-colors hover:bg-black/5 dark:border-white/10 dark:bg-white/3 dark:hover:bg-white/10"
          aria-label="Filter accounts by status"
          @change="handleAccountFilterChange"
        >
          <option v-for="option in ACCOUNT_FILTER_OPTIONS" :key="option.value" :value="option.value">
            {{ option.label }}
          </option>
        </select>

        <select
          v-if="providerFilter === 'codex'"
          :value="quotaSortMode"
          class="h-8 rounded-lg border border-black/10 bg-black/2 px-2 text-xs text-text-primary outline-none transition-colors hover:bg-black/5 dark:border-white/10 dark:bg-white/3 dark:hover:bg-white/10"
          aria-label="Sort Codex quotas by remaining"
          @change="quotaSortMode = ($event.target as HTMLSelectElement).value"
        >
          <option v-for="option in QUOTA_SORT_OPTIONS" :key="option.value" :value="option.value">
            {{ option.label }}
          </option>
        </select>

        <button
          type="button"
          :aria-pressed="expiringFirst"
          :class="`flex h-8 shrink-0 items-center gap-1 rounded-lg border px-2 text-xs transition-colors ${expiringFirst ? 'border-amber-500/40 bg-amber-500/10 text-amber-500' : 'border-black/10 text-text-primary hover:bg-black/5 dark:border-white/10 dark:hover:bg-white/5'}`"
          title="Sort accounts by earliest quota reset time"
          @click="expiringFirst = !expiringFirst"
        >
          <span class="material-symbols-outlined text-[14px]"> hourglass_top </span>
          <span class="hidden sm:inline">Expiring first</span>
        </button>

        <!-- Bulk: disable depleted -->
        <button
          type="button"
          :disabled="bulkToggling"
          class="flex h-8 shrink-0 items-center gap-1 rounded-lg border border-red-500/30 px-2 text-xs text-red-500 transition-colors hover:bg-red-500/10 disabled:opacity-50"
          title="Disable connections with depleted quota on the current page"
          @click="handleDisableDepleted"
        >
          <span class="material-symbols-outlined text-[14px]"> block </span>
          <span class="hidden sm:inline">Turn off Empty</span>
        </button>

        <!-- Bulk: enable available -->
        <button
          type="button"
          :disabled="bulkToggling"
          class="flex h-8 shrink-0 items-center gap-1 rounded-lg border border-emerald-500/30 px-2 text-xs text-emerald-500 transition-colors hover:bg-emerald-500/10 disabled:opacity-50"
          title="Enable connections that still have quota on the current page"
          @click="handleEnableAvailable"
        >
          <span class="material-symbols-outlined text-[14px]"> check_circle </span>
          <span class="hidden sm:inline">Turn on Available</span>
        </button>

        <!-- Auto quota tracker (server-side, default off) -->
        <button
          type="button"
          :aria-pressed="quotaAutoTrackerEnabled"
          class="flex h-8 shrink-0 items-center gap-1 rounded-lg border border-black/10 px-2 text-xs transition-colors hover:bg-black/5 dark:border-white/10 dark:hover:bg-white/5"
          :title="quotaAutoTrackerEnabled ? 'Disable auto quota tracker' : 'Enable auto quota tracker'"
          @click="toggleAutoTracker"
        >
          <span
            :class="`material-symbols-outlined text-[14px] ${quotaAutoTrackerEnabled ? 'text-primary' : 'text-text-muted'}`"
          >
            {{ quotaAutoTrackerEnabled ? "toggle_on" : "toggle_off" }}
          </span>
          <span class="hidden text-text-primary sm:inline">Auto quota tracker</span>
        </button>

        <!-- Auto-refresh toggle -->
        <button
          type="button"
          class="flex h-8 shrink-0 items-center gap-1 rounded-lg border border-black/10 px-2 text-xs transition-colors hover:bg-black/5 dark:border-white/10 dark:hover:bg-white/5"
          :title="autoRefresh ? 'Disable auto-refresh' : 'Enable auto-refresh'"
          @click="autoRefresh = !autoRefresh"
        >
          <span
            :class="`material-symbols-outlined text-[14px] ${autoRefresh ? 'text-primary' : 'text-text-muted'}`"
          >
            {{ autoRefresh ? "toggle_on" : "toggle_off" }}
          </span>
          <span class="hidden text-text-primary sm:inline">Auto-refresh</span>
          <span v-if="autoRefresh" class="text-[10px] text-text-muted tabular-nums">
            ({{ countdown }}s)
          </span>
        </button>

        <!-- Refresh all button -->
        <button
          type="button"
          :disabled="refreshingAll"
          class="flex h-8 shrink-0 items-center gap-1 rounded-lg border border-black/10 px-2 text-xs text-text-primary transition-colors hover:bg-black/5 dark:border-white/10 dark:hover:bg-white/5 disabled:opacity-50"
          title="Refresh all"
          @click="refreshAll()"
        >
          <span
            :class="`material-symbols-outlined text-[14px] ${refreshingAll ? 'animate-spin' : ''}`"
          >
            refresh
          </span>
        </button>
      </div>
    </div>

    <!-- Provider cards: 2 columns, compact -->
    <div
      v-if="expiringFirst"
      class="rounded-xl border border-amber-500/20 bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-300"
    >
      Expiring-first currently reorders accounts inside the current page. Cross-page ordering still
      follows backend pagination.
    </div>

    <div class="grid grid-cols-1 md:grid-cols-2 gap-3">
      <Card
        v-for="conn in sortedConnections"
        :key="conn.id"
        padding="none"
        :class-name="`min-w-0 ${conn.isActive === false ? 'opacity-60' : ''}`"
      >
        <div class="px-3 py-2 border-b border-black/10 dark:border-white/10">
          <div class="flex items-center justify-between gap-2">
            <div class="flex items-center gap-2 min-w-0">
              <div class="w-8 h-8 shrink-0 rounded-md flex items-center justify-center overflow-hidden">
                <ProviderIcon
                  :src="`/providers/${conn.provider}.png`"
                  :alt="conn.provider"
                  :size="32"
                  class-name="object-contain"
                  :fallback-text="conn.provider?.slice(0, 2).toUpperCase() || 'PR'"
                />
              </div>
              <div class="min-w-0">
                <h3 class="text-sm font-semibold text-text-primary truncate">
                  {{ providerLabel(conn.provider ?? "") }}
                </h3>
                <p v-if="getConnectionLabel(conn)" class="text-xs text-text-muted truncate">
                  {{ getConnectionLabel(conn) }}
                </p>
                <p
                  v-if="getConnectionSecondaryLabel(conn)"
                  class="text-[11px] text-text-muted/80 truncate"
                >
                  {{ getConnectionSecondaryLabel(conn) }}
                </p>
              </div>
            </div>

            <div class="flex items-center gap-1 shrink-0">
              <template v-if="conn.provider === 'codex'">
                <Tooltip
                  :text="
                    getCodexResetCreditCount(quotaData[conn.id]) > 0
                      ? `Use one Codex reset credit. Available: ${getCodexResetCreditCount(quotaData[conn.id])}`
                      : 'No Codex reset credits available'
                  "
                >
                  <button
                    type="button"
                    :disabled="
                      getCodexResetCreditCount(quotaData[conn.id]) <= 0 ||
                      !!loading[conn.id] ||
                      deletingId === conn.id ||
                      togglingId === conn.id ||
                      resettingLimitId === conn.id
                    "
                    :aria-label="
                      getCodexResetCreditCount(quotaData[conn.id]) > 0
                        ? `Use one Codex reset credit. ${getCodexResetCreditCount(quotaData[conn.id])} available.`
                        : 'No Codex reset credits available'
                    "
                    :class="`flex h-8 min-w-10 items-center justify-center gap-1 rounded-lg border px-2 text-[11px] font-medium tabular-nums transition-colors focus-visible:outline focus-visible:outline-offset-2 focus-visible:outline-primary/60 disabled:cursor-not-allowed disabled:opacity-60 ${
                      getCodexResetCreditCount(quotaData[conn.id]) > 0
                        ? 'border-primary/30 bg-primary/5 text-primary hover:bg-primary/10'
                        : 'border-black/10 bg-black/2 text-text-muted dark:border-white/10 dark:bg-white/3'
                    }`"
                    @click="
                      resetConfirmState = {
                        connection: conn,
                        resetCreditCount: getCodexResetCreditCount(quotaData[conn.id]),
                      }
                    "
                  >
                    <span
                      :class="`material-symbols-outlined text-[15px] ${resettingLimitId === conn.id ? 'animate-spin' : ''}`"
                    >
                      {{ resettingLimitId === conn.id ? "progress_activity" : "restart_alt" }}
                    </span>
                    <span>{{ getCodexResetCreditCount(quotaData[conn.id]) }}</span>
                  </button>
                </Tooltip>
                <Tooltip text="View Codex reset credit expiry">
                  <button
                    type="button"
                    :disabled="
                      !!loading[conn.id] ||
                      deletingId === conn.id ||
                      togglingId === conn.id ||
                      resettingLimitId === conn.id
                    "
                    aria-label="View Codex reset credit expiry"
                    class="flex h-8 w-8 items-center justify-center rounded-lg border border-black/10 text-text-muted transition-colors hover:bg-black/5 hover:text-primary disabled:cursor-not-allowed disabled:opacity-50 dark:border-white/10 dark:hover:bg-white/5"
                    @click="handleViewCodexResetCredits(conn)"
                  >
                    <span class="material-symbols-outlined text-[17px]">schedule</span>
                  </button>
                </Tooltip>
              </template>

              <Tooltip
                v-if="AUTO_PING_SETTINGS_KEYS[conn.provider ?? ''] && conn.authType === 'oauth'"
                :text="AUTO_PING_TOOLTIPS[conn.provider ?? '']"
              >
                <button
                  type="button"
                  aria-label="Toggle auto-ping"
                  :class="`flex h-8 w-8 items-center justify-center rounded-lg transition-colors hover:bg-black/5 dark:hover:bg-white/5 ${autoPingMaps[conn.provider as 'codex']?.[conn.id] === true ? 'text-primary' : 'text-text-muted'}`"
                  @click="
                    toggleAutoPing(
                      conn.id,
                      conn.provider ?? '',
                      !(autoPingMaps[conn.provider as 'codex']?.[conn.id] === true),
                    )
                  "
                >
                  <span class="material-symbols-outlined text-[18px]">bolt</span>
                </button>
              </Tooltip>

              <Tooltip text="Refresh quota">
                <button
                  type="button"
                  :disabled="
                    !!loading[conn.id] ||
                    deletingId === conn.id ||
                    togglingId === conn.id ||
                    resettingLimitId === conn.id
                  "
                  aria-label="Refresh quota"
                  class="flex h-8 w-8 items-center justify-center rounded-lg hover:bg-black/5 dark:hover:bg-white/5 transition-colors disabled:opacity-50"
                  @click="refreshProvider(conn.id, conn.provider ?? '')"
                >
                  <span
                    :class="`material-symbols-outlined text-[18px] text-text-muted ${loading[conn.id] ? 'animate-spin' : ''}`"
                  >
                    refresh
                  </span>
                </button>
              </Tooltip>
              <Tooltip text="Edit connection">
                <button
                  type="button"
                  :disabled="
                    deletingId === conn.id ||
                    togglingId === conn.id ||
                    resettingLimitId === conn.id
                  "
                  aria-label="Edit connection"
                  class="flex h-8 w-8 items-center justify-center rounded-lg hover:bg-black/5 dark:hover:bg-white/5 text-text-muted hover:text-primary transition-colors disabled:opacity-50"
                  @click="
                    selectedConnection = conn;
                    showEditModal = true;
                  "
                >
                  <span class="material-symbols-outlined text-[18px]"> edit </span>
                </button>
              </Tooltip>
              <Tooltip text="Delete connection">
                <button
                  type="button"
                  :disabled="
                    deletingId === conn.id ||
                    togglingId === conn.id ||
                    resettingLimitId === conn.id
                  "
                  aria-label="Delete connection"
                  class="flex h-8 w-8 items-center justify-center rounded-lg hover:bg-red-500/10 text-red-500 transition-colors disabled:opacity-50"
                  @click="handleDeleteConnection(conn.id)"
                >
                  <span
                    :class="`material-symbols-outlined text-[18px] ${deletingId === conn.id ? 'animate-pulse' : ''}`"
                  >
                    delete
                  </span>
                </button>
              </Tooltip>
              <div
                class="inline-flex items-center pl-0.5"
                :title="(conn.isActive ?? true) ? 'Disable connection' : 'Enable connection'"
              >
                <Toggle
                  size="sm"
                  :model-value="conn.isActive ?? true"
                  :disabled="
                    deletingId === conn.id ||
                    togglingId === conn.id ||
                    resettingLimitId === conn.id
                  "
                  @update:model-value="(v: boolean) => handleToggleConnectionActive(conn.id, v)"
                />
              </div>
            </div>
          </div>
        </div>

        <div class="px-2 py-1.5">
          <div v-if="loading[conn.id]" class="text-center py-5 text-text-muted">
            <span class="material-symbols-outlined text-[28px] animate-spin"> progress_activity </span>
          </div>
          <div v-else-if="errors[conn.id]" class="text-center py-5">
            <span class="material-symbols-outlined text-[28px] text-red-500"> error </span>
            <p class="mt-1.5 text-xs text-text-muted">{{ errors[conn.id] }}</p>
          </div>
          <div v-else-if="quotaData[conn.id]?.message" class="text-center py-5">
            <p class="text-xs text-text-muted">{{ quotaData[conn.id]?.message }}</p>
          </div>
          <QuotaTable
            v-else
            :quotas="filterQuotasByVisibility(conn.provider ?? '', quotaData[conn.id]?.quotas || [], quotaVisibility)"
            compact
            sort-mode="default"
            :show-sort-label="conn.provider === 'codex' && quotaSortMode !== 'default'"
            :on-hide-quota="(quotaRow: NormalizedQuota) => handleHideQuota(conn.provider ?? '', quotaRow)"
          />
          <p
            v-if="quotaData[conn.id]?.message && !errors[conn.id] && !loading[conn.id]"
            class="mt-2 px-1 text-[10px] leading-relaxed text-text-muted"
          >
            {{ quotaData[conn.id]?.message }}
          </p>
          <div
            v-if="getHiddenQuotaRows(conn.provider ?? '', quotaData[conn.id]?.quotas || [], quotaVisibility).length > 0"
            class="mt-2 flex min-w-0 items-center gap-1 border-t border-black/5 pt-2 text-[10px] text-text-muted dark:border-white/5"
          >
            <span class="material-symbols-outlined shrink-0 text-[14px]"> visibility_off </span>
            <span class="shrink-0">Hidden:</span>
            <div class="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto whitespace-nowrap pb-2">
              <button
                v-for="quotaRow in getHiddenQuotaRows(
                  conn.provider ?? '',
                  quotaData[conn.id]?.quotas || [],
                  quotaVisibility,
                )"
                :key="getQuotaVisibilityKey(quotaRow)"
                type="button"
                class="shrink-0 rounded-md border border-black/10 px-1.5 py-0.5 transition-colors hover:bg-black/5 hover:text-text-primary dark:border-white/10 dark:hover:bg-white/5"
                title="Show this quota row"
                @click="handleShowQuota(conn.provider ?? '', quotaRow)"
              >
                {{ quotaRow.name }}
              </button>
            </div>
          </div>
        </div>
      </Card>
    </div>

    <div
      class="rounded-xl border border-black/10 bg-black/2 px-3 py-2 dark:border-white/10 dark:bg-white/3"
    >
      <div class="flex flex-wrap items-center justify-between gap-2">
        <span class="text-xs text-text-muted">{{ connectionsPageSummary }}</span>
        <div class="flex flex-wrap items-center gap-2">
          <select
            :value="isCustomPageSize ? 'custom' : String(pageSize)"
            class="h-8 rounded-lg border border-black/10 bg-black/2 px-2 text-xs text-text-primary outline-none transition-colors hover:bg-black/5 dark:border-white/10 dark:bg-white/3 dark:hover:bg-white/10"
            aria-label="Accounts per page"
            @change="handlePageSizeSelect"
          >
            <option v-for="option in ACCOUNT_PAGE_SIZE_OPTIONS" :key="option" :value="String(option)">
              {{ option }} / page
            </option>
            <option value="custom">Custom</option>
          </select>
          <input
            v-model="customPageSizeInput"
            type="number"
            min="1"
            :max="String(ACCOUNT_PAGE_SIZE_MAX)"
            inputmode="numeric"
            class="h-8 w-20 rounded-lg border border-black/10 bg-black/2 px-2 text-xs text-text-primary outline-none transition-colors hover:bg-black/5 dark:border-white/10 dark:bg-white/3 dark:hover:bg-white/10"
            aria-label="Custom accounts per page"
            placeholder="Custom"
            @blur="applyCustomPageSize"
            @keydown.enter="applyCustomPageSize"
          />
          <span class="text-xs text-text-muted">
            Page {{ pagination.page }} / {{ pagination.totalPages }}
          </span>
        </div>
        <div class="flex items-center gap-1.5">
          <button
            type="button"
            :disabled="pagination.page <= 1 || connectionsLoading || refreshingAll"
            class="flex h-8 items-center rounded-lg border border-black/10 px-3 text-xs text-text-primary transition-colors hover:bg-black/5 disabled:cursor-not-allowed disabled:opacity-40 dark:border-white/10 dark:hover:bg-white/5"
            @click="page = 1"
          >
            First Page
          </button>
          <button
            type="button"
            :disabled="pagination.page <= 1 || connectionsLoading || refreshingAll"
            class="flex h-8 w-8 items-center justify-center rounded-lg border border-black/10 text-text-primary transition-colors hover:bg-black/5 disabled:cursor-not-allowed disabled:opacity-40 dark:border-white/10 dark:hover:bg-white/5"
            aria-label="Previous accounts page"
            @click="page = Math.max(1, page - 1)"
          >
            <span class="material-symbols-outlined text-[16px]"> chevron_left </span>
          </button>
          <button
            type="button"
            :disabled="
              pagination.page >= pagination.totalPages || connectionsLoading || refreshingAll
            "
            class="flex h-8 w-8 items-center justify-center rounded-lg border border-black/10 text-text-primary transition-colors hover:bg-black/5 disabled:cursor-not-allowed disabled:opacity-40 dark:border-white/10 dark:hover:bg-white/5"
            aria-label="Next accounts page"
            @click="page = Math.min(pagination.totalPages, page + 1)"
          >
            <span class="material-symbols-outlined text-[16px]"> chevron_right </span>
          </button>
          <button
            type="button"
            :disabled="
              pagination.page >= pagination.totalPages || connectionsLoading || refreshingAll
            "
            class="flex h-8 items-center rounded-lg border border-black/10 px-3 text-xs text-text-primary transition-colors hover:bg-black/5 disabled:cursor-not-allowed disabled:opacity-40 dark:border-white/10 dark:hover:bg-white/5"
            @click="page = pagination.totalPages"
          >
            Last Page
          </button>
        </div>
      </div>
    </div>

    <ConfirmModal
      :is-open="Boolean(resetConfirmState)"
      title="Reset Codex limit?"
      :message="`Use 1 Codex reset credit for ${getConnectionLabel((resetConfirmState?.connection || {}) as Connection) || 'this account'}. This cannot be undone. Remaining credits: ${resetConfirmState?.resetCreditCount ?? 0}.`"
      confirm-text="Reset limit"
      cancel-text="Cancel"
      variant="danger"
      :loading="Boolean(resettingLimitId)"
      @close="
        () => {
          if (!resettingLimitId) resetConfirmState = null;
        }
      "
      @confirm="
        async () => {
          const connection = resetConfirmState?.connection;
          if (!connection) return;
          await handleResetCodexLimit(connection.id, connection.provider ?? '');
          resetConfirmState = null;
        }
      "
    />

    <div
      v-if="resetCreditsState"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 px-4 backdrop-blur-sm"
    >
      <div
        class="w-full max-w-2xl overflow-hidden rounded-2xl border border-black/15 bg-white shadow-2xl ring-1 ring-black/10 dark:border-white/15 dark:bg-neutral-950 dark:ring-white/10"
      >
        <div
          class="flex items-start justify-between gap-3 border-b border-black/10 bg-black/3 px-4 py-3 dark:border-white/10 dark:bg-white/4"
        >
          <div class="min-w-0">
            <h3 class="text-base font-semibold text-text-primary">Codex Reset Credit Expiry</h3>
            <p class="mt-0.5 truncate text-xs text-text-muted">
              {{ getConnectionLabel(resetCreditsState.connection) || "Codex account" }}
            </p>
          </div>
          <button
            type="button"
            class="flex h-8 w-8 items-center justify-center rounded-lg text-text-muted transition-colors hover:bg-black/5 hover:text-text-primary dark:hover:bg-white/5"
            aria-label="Close reset credit expiry modal"
            @click="resetCreditsState = null"
          >
            <span class="material-symbols-outlined text-[18px]">close</span>
          </button>
        </div>

        <div class="max-h-[70vh] overflow-auto bg-white p-4 dark:bg-neutral-950">
          <div
            v-if="resetCreditsState.loading"
            class="flex items-center justify-center gap-2 py-10 text-sm text-text-muted"
          >
            <span class="material-symbols-outlined animate-spin text-[20px]">progress_activity</span>
            Loading reset credits...
          </div>
          <div
            v-else-if="resetCreditsState.error"
            class="rounded-xl border border-red-500/20 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"
          >
            {{ resetCreditsState.error }}
          </div>
          <div v-else-if="resetCreditsState.data?.credits?.length" class="space-y-3">
            <div
              class="flex items-center justify-between rounded-xl border border-black/10 bg-black/2 px-3 py-2 text-xs text-text-muted dark:border-white/10 dark:bg-white/3"
            >
              <span>
                {{ resetCreditsState.data.credits.length }} reset credit{{
                  resetCreditsState.data.credits.length === 1 ? "" : "s"
                }}
              </span>
              <span>{{ resetCreditsState.data.availableCount ?? 0 }} available</span>
            </div>
            <div class="overflow-x-auto rounded-xl border border-black/10 dark:border-white/10">
              <table class="w-full min-w-140 text-left text-sm">
                <thead
                  class="bg-black/3 text-xs uppercase tracking-wide text-text-muted dark:bg-white/4"
                >
                  <tr>
                    <th class="px-3 py-2 font-medium">Status</th>
                    <th class="px-3 py-2 font-medium">Granted At</th>
                    <th class="px-3 py-2 font-medium">Expires At</th>
                    <th class="px-3 py-2 font-medium">Remaining</th>
                  </tr>
                </thead>
                <tbody>
                  <tr
                    v-for="(credit, index) in resetCreditsState.data.credits"
                    :key="`${credit.status}-${credit.expiresAt || index}`"
                    class="border-t border-black/5 dark:border-white/5"
                  >
                    <td class="px-3 py-2">
                      <span
                        class="rounded-full bg-primary/10 px-2 py-0.5 text-xs font-medium text-primary"
                      >
                        {{ credit.status || "unknown" }}
                      </span>
                    </td>
                    <td class="px-3 py-2 text-text-muted">{{ formatCreditDate(credit.grantedAt) }}</td>
                    <td class="px-3 py-2 text-text-primary">{{ formatCreditDate(credit.expiresAt) }}</td>
                    <td class="px-3 py-2 font-medium text-text-primary">
                      {{ formatTimeRemaining(credit.expiresAt) }}
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
          </div>
          <div
            v-else
            class="rounded-xl border border-black/10 bg-black/2 px-3 py-8 text-center text-sm text-text-muted dark:border-white/10 dark:bg-white/3"
          >
            No reset credit details returned for this account.
          </div>
        </div>
      </div>
    </div>

    <EditConnectionModal
      :is-open="showEditModal"
      :connection="selectedConnection"
      :proxy-pools="proxyPools"
      @save="handleUpdateConnection"
      @close="
        () => {
          showEditModal = false;
          selectedConnection = null;
        }
      "
    />
  </div>
</template>
