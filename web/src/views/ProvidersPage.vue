<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";

import CardSkeleton from "@/components/ui/CardSkeleton.vue";
import { useProviders } from "@/constants/providers";
import { useHeaderSearchStore } from "@/stores/headerSearch";
import { useNotificationStore } from "@/stores/notification";
import { getErrorCode, getRelativeTime } from "@/utils";
import AddCompatibleModal from "./providers/components/AddCompatibleModal.vue";
import ModelAvailabilityBadge from "./providers/components/ModelAvailabilityBadge.vue";
import ProviderCard from "./providers/components/ProviderCard.vue";

const STATUS_FILTER_OPTIONS = [
	{ value: "all", label: "All" },
	{ value: "active", label: "Active" },
	{ value: "inactive", label: "Inactive" },
	{ value: "none", label: "No connection" },
];

// noAuth providers (e.g. free proxies) are always usable even though they
// never have a stored connection record, so they never fall into "none".
function getConnectionStatus(
	stats: Record<string, any> | null | undefined,
	isNoAuth = false,
): string {
	if (isNoAuth) return "active";
	if (!stats || stats.total === 0) return "none";
	return stats.allDisabled ? "inactive" : "active";
}

function matchesStatusFilter(
	statusFilter: string,
	stats: Record<string, any> | null | undefined,
	isNoAuth = false,
): boolean {
	if (statusFilter === "all") return true;
	return getConnectionStatus(stats, isNoAuth) === statusFilter;
}

const APIKEY_INITIAL_VISIBLE = 20;

const {
	OAUTH_PROVIDERS,
	FREE_PROVIDERS,
	FREE_TIER_PROVIDERS,
	APIKEY_PROVIDERS,
} = useProviders();

const connections = ref<Array<Record<string, any>>>([]);
const providerNodes = ref<Array<Record<string, any>>>([]);
const loading = ref(true);
const showAllApikey = ref(false);
const showAddCompatibleModal = ref(false);
const showAddAnthropicCompatibleModal = ref(false);
const testingMode = ref<string | null>(null);
const testResults = ref<Record<string, any> | null>(null);
const statusFilter = ref("all");

const notify = useNotificationStore();
const headerSearch = useHeaderSearchStore();

const searchQuery = computed(() => headerSearch.query);

onMounted(() => {
	headerSearch.register("Search providers...");
	fetchData();
});

onBeforeUnmount(() => {
	headerSearch.unregister();
});

function matchSearch(name: string | undefined): boolean {
	if (!searchQuery.value.trim()) return true;
	if (!name) return false;
	return name.toLowerCase().includes(searchQuery.value.trim().toLowerCase());
}

function getConnectionErrorTag(connection: Record<string, any> | null): string | null {
	if (!connection) return null;

	const explicitType = connection.lastErrorType;
	if (explicitType === "runtime_error") return "RUNTIME";
	if (
		explicitType === "upstream_auth_error" ||
		explicitType === "auth_missing" ||
		explicitType === "token_refresh_failed" ||
		explicitType === "token_expired"
	)
		return "AUTH";
	if (explicitType === "upstream_rate_limited") return "429";
	if (explicitType === "upstream_unavailable") return "5XX";
	if (explicitType === "network_error") return "NET";

	const numericCode = Number(connection.errorCode);
	if (Number.isFinite(numericCode) && numericCode >= 400) return String(numericCode);

	const fromMessage = getErrorCode(connection.lastError);
	if (fromMessage === "401" || fromMessage === "403") return "AUTH";
	if (fromMessage && fromMessage !== "ERR") return fromMessage;

	const msg = (connection.lastError || "").toLowerCase();
	if (
		msg.includes("runtime") ||
		msg.includes("not runnable") ||
		msg.includes("not installed")
	)
		return "RUNTIME";
	if (
		msg.includes("invalid api key") ||
		msg.includes("token invalid") ||
		msg.includes("revoked") ||
		msg.includes("unauthorized")
	)
		return "AUTH";

	return "ERR";
}

function getProviderStats(providerId: string, authType: string | string[]) {
	const authTypes = Array.isArray(authType) ? authType : [authType];
	const providerConnections = connections.value.filter(
		(c) => c.provider === providerId && authTypes.includes(c.authType),
	);

	const getEffectiveStatus = (conn: Record<string, any>) => {
		const isCooldown = Object.entries(conn).some(
			([k, v]) =>
				k.startsWith("modelLock_") && v && new Date(v as string).getTime() > Date.now(),
		);
		return conn.testStatus === "unavailable" && !isCooldown
			? "active"
			: conn.testStatus;
	};

	const connected = providerConnections.filter((c) => {
		const status = getEffectiveStatus(c);
		return status === "active" || status === "success";
	}).length;

	const errorConns = providerConnections.filter((c) => {
		const status = getEffectiveStatus(c);
		return status === "error" || status === "expired" || status === "unavailable";
	});

	const error = errorConns.length;
	const total = providerConnections.length;
	const allDisabled =
		total > 0 && providerConnections.every((c) => c.isActive === false);

	const latestError = [...errorConns].sort(
		(a, b) =>
			new Date(b.lastErrorAt || 0).getTime() - new Date(a.lastErrorAt || 0).getTime(),
	)[0];
	const errorCode = latestError ? getConnectionErrorTag(latestError) : null;
	const errorTime = latestError?.lastErrorAt
		? getRelativeTime(latestError.lastErrorAt)
		: null;

	return { connected, error, total, errorCode, errorTime, allDisabled };
}

function matchStatus(
	stats: Record<string, any>,
	isNoAuth?: boolean,
): boolean {
	return matchesStatusFilter(statusFilter.value, stats, isNoAuth);
}

async function fetchData() {
	try {
		const [connectionsRes, nodesRes] = await Promise.all([
			fetch("/api/providers"),
			fetch("/api/provider-nodes"),
		]);
		const connectionsData = await connectionsRes.json();
		const nodesData = await nodesRes.json();
		if (connectionsRes.ok) connections.value = connectionsData.connections || [];
		if (nodesRes.ok) providerNodes.value = nodesData.nodes || [];
	} catch (error) {
		console.log("Error fetching data:", error);
	} finally {
		loading.value = false;
	}
}

function sortByPriority(
	entries: Array<[string, Record<string, any>]>,
	authType: string | string[],
): Array<[string, Record<string, any>]> {
	return [...entries].sort(([ka, a], [kb, b]) => {
		const pa = a.priority ?? 999;
		const pb = b.priority ?? 999;
		if (pa !== pb) return pa - pb;
		const sa = getProviderStats(ka, authType);
		const sb = getProviderStats(kb, authType);
		const ca = sa.connected > 0 ? 1 : 0;
		const cb = sb.connected > 0 ? 1 : 0;
		if (ca !== cb) return cb - ca;
		return (a.name || "").localeCompare(b.name || "");
	});
}

// Toggle all connections for a provider on/off. authType may be a single
// string or an array.
async function handleToggleProvider(
	providerId: string,
	authType: string | string[],
	newActive: boolean,
) {
	const authTypes = Array.isArray(authType) ? authType : [authType];
	const matches = (c: Record<string, any>) =>
		c.provider === providerId && authTypes.includes(c.authType);
	const providerConns = connections.value.filter(matches);
	connections.value = connections.value.map((c) =>
		matches(c) ? { ...c, isActive: newActive } : c,
	);
	await Promise.allSettled(
		providerConns.map((c) =>
			fetch(`/api/providers/${c.id}`, {
				method: "PUT",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ isActive: newActive }),
			}),
		),
	);
}

async function handleBatchTest(mode: string, providerId: string | null = null) {
	if (testingMode.value) return;
	testingMode.value = mode === "provider" ? providerId : mode;
	testResults.value = null;
	try {
		const res = await fetch("/api/providers/test-batch", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ mode, providerId }),
		});
		const data = await res.json();
		testResults.value = data;
		if (data.summary) {
			const { passed, failed, total } = data.summary;
			if (failed === 0) notify.success(`All ${total} tests passed`);
			else notify.warning(`${passed}/${total} passed, ${failed} failed`);
		}
	} catch {
		testResults.value = { error: "Test request failed" };
		notify.error("Provider test failed");
	} finally {
		testingMode.value = null;
	}
}

// Dual-auth providers (oauth + apikey) store API keys as authType "apikey"
// (and sometimes "api_key"). Card stats must count both so totals match detail.
function dualAuthTypes(
	info: Record<string, any> | undefined,
	key: string,
): string | string[] {
	const modes = info?.authModes;
	// Free-tier and API-key providers default to supporting apikey even when the
	// registry entry omits authModes (e.g. deepseek, mistral, nvidia) — otherwise
	// their apikey connections are invisible on the grid card.
	if (!Array.isArray(modes)) {
		return key in FREE_TIER_PROVIDERS || key in APIKEY_PROVIDERS
			? ["oauth", "apikey", "api_key"]
			: "oauth";
	}
	if (!modes.includes("apikey")) return "oauth";
	return ["oauth", "apikey", "api_key"];
}

const compatibleProviders = computed(() =>
	providerNodes.value
		.filter((node) => node.type === "openai-compatible")
		.map((node) => ({
			id: node.id,
			name: node.name || "OpenAI Compatible",
			color: "#10A37F",
			textIcon: "OC",
			apiType: node.apiType,
		}))
		.filter(
			(p) => matchSearch(p.name) && matchStatus(getProviderStats(p.id, "apikey")),
		),
);

const anthropicCompatibleProviders = computed(() =>
	providerNodes.value
		.filter((node) => node.type === "anthropic-compatible")
		.map((node) => ({
			id: node.id,
			name: node.name || "Anthropic Compatible",
			color: "#D97757",
			textIcon: "AC",
		}))
		.filter(
			(p) => matchSearch(p.name) && matchStatus(getProviderStats(p.id, "apikey")),
		),
);

const oauthEntries = computed(() =>
	sortByPriority(
		Object.entries(OAUTH_PROVIDERS).filter(
			([key, info]) =>
				!info.hidden &&
				matchSearch(info.name) &&
				matchStatus(getProviderStats(key, dualAuthTypes(info, key)), info.noAuth),
		),
		"oauth",
	),
);

const freeEntries = computed(() =>
	Object.entries(FREE_PROVIDERS)
		.filter(
			([key, info]) =>
				!info.hidden &&
				matchSearch(info.name) &&
				matchStatus(getProviderStats(key, dualAuthTypes(info, key)), info.noAuth),
		)
		.sort(([, a], [, b]) => (b.noAuth ? 1 : 0) - (a.noAuth ? 1 : 0)),
);

// Free Tier cards may be oauth-only (e.g. kimchi) or dual-auth, so count via
// dualAuthTypes per provider instead of a fixed "apikey" — otherwise oauth
// connections are invisible here (mismatch with the detail page).
const freeTierEntries = computed(() =>
	Object.entries(FREE_TIER_PROVIDERS)
		.filter(
			([key, info]) =>
				!info.hidden &&
				matchSearch(info.name) &&
				(info.serviceKinds ?? ["llm"]).includes("llm") &&
				matchStatus(getProviderStats(key, dualAuthTypes(info, key)), info.noAuth),
		)
		.sort(([ka, a], [kb, b]) => {
			const pa = a.priority ?? 999;
			const pb = b.priority ?? 999;
			if (pa !== pb) return pa - pb;
			const noAuthDiff = (b.noAuth ? 1 : 0) - (a.noAuth ? 1 : 0);
			if (noAuthDiff !== 0) return noAuthDiff;
			const ca = getProviderStats(ka, dualAuthTypes(a, ka)).connected > 0 ? 0 : 1;
			const cb = getProviderStats(kb, dualAuthTypes(b, kb)).connected > 0 ? 0 : 1;
			if (ca !== cb) return ca - cb;
			return (a.name || "").localeCompare(b.name || "");
		}),
);

// API Key: connected providers first, then alphabetical by name
const apikeyEntries = computed(() =>
	Object.entries(APIKEY_PROVIDERS)
		.filter(
			([key, info]) =>
				!info.hidden &&
				(info.serviceKinds ?? ["llm"]).includes("llm") &&
				matchSearch(info.name) &&
				matchStatus(getProviderStats(key, "apikey"), info.noAuth),
		)
		.sort(([ka, a], [kb, b]) => {
			const ca = getProviderStats(ka, "apikey").total > 0 ? 0 : 1;
			const cb = getProviderStats(kb, "apikey").total > 0 ? 0 : 1;
			if (ca !== cb) return ca - cb;
			return (a.name || "").localeCompare(b.name || "");
		}),
);

const isApikeySearching = computed(
	() => !!searchQuery.value.trim() || statusFilter.value !== "all",
);
const visibleApikeyEntries = computed(() =>
	isApikeySearching.value || showAllApikey.value
		? apikeyEntries.value
		: apikeyEntries.value.slice(0, APIKEY_INITIAL_VISIBLE),
);
const hiddenApikeyCount = computed(
	() => apikeyEntries.value.length - APIKEY_INITIAL_VISIBLE,
);

const hasAnyResult = computed(
	() =>
		oauthEntries.value.length > 0 ||
		freeEntries.value.length > 0 ||
		freeTierEntries.value.length > 0 ||
		apikeyEntries.value.length > 0 ||
		compatibleProviders.value.length > 0 ||
		anthropicCompatibleProviders.value.length > 0,
);

function onCompatibleCreated(node: Record<string, any>, variant: "openai" | "anthropic") {
	providerNodes.value = [...providerNodes.value, node];
	if (variant === "openai") showAddCompatibleModal.value = false;
	else showAddAnthropicCompatibleModal.value = false;
}

const testModeLabel = computed(() => {
	const mode = testResults.value?.mode;
	const labels: Record<string, string> = {
		oauth: "OAuth",
		free: "Free",
		apikey: "API Key",
		provider: "Provider",
		all: "All",
	};
	return labels[mode] || mode;
});
</script>

<template>
  <div v-if="loading" class="flex flex-col gap-8">
    <CardSkeleton />
    <CardSkeleton />
  </div>

  <div v-else class="flex min-w-0 flex-col gap-6 px-1 sm:px-0">
    <div class="flex items-center justify-end">
      <select
        v-model="statusFilter"
        class="h-8 rounded-lg border border-black/10 bg-black/2 px-2 text-xs text-text-primary outline-none transition-colors hover:bg-black/5 dark:border-white/10 dark:bg-white/3 dark:hover:bg-white/10"
        aria-label="Filter providers by connection status"
      >
        <option v-for="option in STATUS_FILTER_OPTIONS" :key="option.value" :value="option.value">
          {{ option.label }}
        </option>
      </select>
    </div>

    <div v-if="!hasAnyResult" class="text-center py-8 border border-dashed border-border rounded-xl">
      <span class="material-symbols-outlined text-[32px] text-text-muted mb-2">search_off</span>
      <p class="text-text-muted text-sm">No providers match your search or filters</p>
    </div>

    <!-- Custom Providers (OpenAI/Anthropic Compatible) — dynamic -->
    <div class="flex flex-col gap-4">
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <h2 class="text-lg sm:text-xl font-semibold flex items-center gap-2 leading-tight">
          Custom Providers (OpenAI/Anthropic Compatible){{ " " }}
        </h2>
        <div class="grid grid-cols-1 gap-2 sm:flex sm:w-auto">
          <button
            type="button"
            class="inline-flex h-7 items-center justify-center gap-2 rounded-lg bg-brand-500 px-3 text-xs font-semibold text-white shadow-sm transition-all duration-150 ease-out hover:bg-brand-600 active:scale-[0.97] w-full sm:w-auto"
            @click="showAddAnthropicCompatibleModal = true"
          >
            <span class="material-symbols-outlined text-[18px]">add</span>
            Add Anthropic Compatible
          </button>
          <button
            type="button"
            class="inline-flex h-7 items-center justify-center gap-2 rounded-lg bg-surface-2 px-3 text-xs font-semibold text-text-main border border-border transition-all duration-150 ease-out hover:bg-surface-3 active:scale-[0.97] w-full sm:w-auto"
            @click="showAddCompatibleModal = true"
          >
            <span class="material-symbols-outlined text-[18px]">add</span>
            Add OpenAI Compatible
          </button>
        </div>
      </div>
      <div
        v-if="compatibleProviders.length === 0 && anthropicCompatibleProviders.length === 0"
        class="flex items-center justify-center gap-2 py-2 border border-dashed border-border rounded-xl text-text-muted text-sm"
      >
        <span class="material-symbols-outlined text-[18px]">extension</span>
        <span>No custom providers — use buttons above to add OpenAI/Anthropic compatible endpoints</span>
      </div>
      <div v-else class="grid grid-cols-1 gap-3 sm:grid-cols-2 sm:gap-4 lg:grid-cols-3 xl:grid-cols-4">
        <ProviderCard
          v-for="info in [...compatibleProviders, ...anthropicCompatibleProviders]"
          :key="info.id"
          :provider-id="info.id"
          :provider="info"
          :stats="getProviderStats(info.id, 'apikey')"
          api-key
          @toggle="(active: boolean) => handleToggleProvider(info.id, 'apikey', active)"
        />
      </div>
    </div>

    <!-- OAuth Providers -->
    <div v-if="oauthEntries.length > 0" class="flex flex-col gap-4">
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <h2 class="text-lg sm:text-xl font-semibold flex items-center gap-2 leading-tight">
          OAuth Providers
        </h2>
        <div class="flex w-full flex-col gap-2 sm:w-auto sm:flex-row sm:items-center">
          <ModelAvailabilityBadge />
          <button
            type="button"
            :disabled="!!testingMode"
            :class="`flex w-full items-center justify-center gap-1.5 rounded-lg border px-3 py-2 text-xs font-medium transition-colors sm:w-auto sm:py-1.5 ${
              testingMode === 'oauth'
                ? 'bg-primary/20 border-primary/40 text-primary animate-pulse'
                : 'bg-bg border-border text-text-muted hover:text-text-main hover:border-primary/40'
            }`"
            title="Test all OAuth connections"
            aria-label="Test all OAuth connections"
            @click="handleBatchTest('oauth')"
          >
            <span :class="`material-symbols-outlined text-[14px]${testingMode === 'oauth' ? ' animate-spin' : ''}`">
              play_arrow
            </span>
            {{ testingMode === "oauth" ? "Testing..." : "Test All" }}
          </button>
        </div>
      </div>
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-2 sm:gap-4 lg:grid-cols-3 xl:grid-cols-4">
        <ProviderCard
          v-for="[key, info] in oauthEntries"
          :key="key"
          :provider-id="key"
          :provider="info"
          :stats="getProviderStats(key, dualAuthTypes(info, key))"
          @toggle="(active: boolean) => handleToggleProvider(key, dualAuthTypes(info, key), active)"
        />
      </div>
    </div>

    <!-- Free Tier Providers -->
    <div v-if="freeEntries.length > 0 || freeTierEntries.length > 0" class="flex flex-col gap-4">
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <h2 class="text-lg sm:text-xl font-semibold flex items-center gap-2 leading-tight">
          Free Tier Providers
        </h2>
        <button
          type="button"
          :disabled="!!testingMode"
          :class="`flex w-full items-center justify-center gap-1.5 rounded-lg border px-3 py-2 text-xs font-medium transition-colors sm:w-auto sm:py-1.5 ${
            testingMode === 'free'
              ? 'bg-primary/20 border-primary/40 text-primary animate-pulse'
              : 'bg-bg border-border text-text-muted hover:text-text-main hover:border-primary/40'
          }`"
          title="Test all Free connections"
          aria-label="Test all Free provider connections"
          @click="handleBatchTest('free')"
        >
          <span :class="`material-symbols-outlined text-[14px]${testingMode === 'free' ? ' animate-spin' : ''}`">
            play_arrow
          </span>
          {{ testingMode === "free" ? "Testing..." : "Test All" }}
        </button>
      </div>
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-2 sm:gap-4 lg:grid-cols-3 xl:grid-cols-4">
        <ProviderCard
          v-for="[key, info] in freeEntries"
          :key="key"
          :provider-id="key"
          :provider="info"
          :stats="getProviderStats(key, dualAuthTypes(info, key))"
          @toggle="(active: boolean) => handleToggleProvider(key, dualAuthTypes(info, key), active)"
        />
        <ProviderCard
          v-for="[key, info] in freeTierEntries"
          :key="key"
          :provider-id="key"
          :provider="info"
          :stats="getProviderStats(key, dualAuthTypes(info, key))"
          api-key
          @toggle="(active: boolean) => handleToggleProvider(key, dualAuthTypes(info, key), active)"
        />
      </div>
    </div>

    <!-- API Key Providers — fixed list -->
    <div v-if="apikeyEntries.length > 0" class="flex flex-col gap-4">
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <h2 class="text-lg sm:text-xl font-semibold flex items-center gap-2 leading-tight">
          API Key Providers{{ " " }}
        </h2>
        <button
          type="button"
          :disabled="!!testingMode"
          :class="`flex w-full items-center justify-center gap-1.5 rounded-lg border px-3 py-2 text-xs font-medium transition-colors sm:w-auto sm:py-1.5 ${
            testingMode === 'apikey'
              ? 'bg-primary/20 border-primary/40 text-primary animate-pulse'
              : 'bg-bg border-border text-text-muted hover:text-text-main hover:border-primary/40'
          }`"
          title="Test all API Key connections"
          aria-label="Test all API Key connections"
          @click="handleBatchTest('apikey')"
        >
          <span :class="`material-symbols-outlined text-[14px]${testingMode === 'apikey' ? ' animate-spin' : ''}`">
            play_arrow
          </span>
          {{ testingMode === "apikey" ? "Testing..." : "Test All" }}
        </button>
      </div>
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-2 sm:gap-4 lg:grid-cols-3 xl:grid-cols-4">
        <ProviderCard
          v-for="[key, info] in visibleApikeyEntries"
          :key="key"
          :provider-id="key"
          :provider="info"
          :stats="getProviderStats(key, 'apikey')"
          api-key
          @toggle="(active: boolean) => handleToggleProvider(key, 'apikey', active)"
        />
      </div>
      <button
        v-if="!isApikeySearching && !showAllApikey && hiddenApikeyCount > 0"
        type="button"
        class="flex w-full items-center justify-center gap-1.5 rounded-lg border border-dashed border-primary/40 px-3 py-2.5 text-sm font-medium text-primary transition-colors hover:border-primary hover:bg-primary/5"
        @click="showAllApikey = true"
      >
        <span class="material-symbols-outlined text-[16px]">expand_more</span>
        Show all {{ apikeyEntries.length }} providers
      </button>
    </div>

    <AddCompatibleModal
      variant="openai"
      :is-open="showAddCompatibleModal"
      @close="showAddCompatibleModal = false"
      @created="(node: Record<string, any>) => onCompatibleCreated(node, 'openai')"
    />
    <AddCompatibleModal
      variant="anthropic"
      :is-open="showAddAnthropicCompatibleModal"
      @close="showAddAnthropicCompatibleModal = false"
      @created="(node: Record<string, any>) => onCompatibleCreated(node, 'anthropic')"
    />

    <!-- Test Results Modal -->
    <div
      v-if="testResults"
      class="fixed inset-0 z-50 flex items-start justify-center px-3 pt-[6vh] sm:pt-[10vh]"
    >
      <button
        type="button"
        class="absolute inset-0 bg-black/60 backdrop-blur-sm"
        aria-label="Close test results"
        @click="testResults = null"
      />
      <div
        class="relative bg-surface border border-border rounded-xl w-full max-w-150 max-h-[86vh] sm:max-h-[80vh] overflow-y-auto shadow-2xl"
      >
        <div class="sticky top-0 z-10 flex items-center justify-between px-5 py-3 border-b border-border bg-surface/95 backdrop-blur-sm rounded-t-xl">
          <h3 class="font-semibold">Test Results</h3>
          <button
            type="button"
            class="p-1 rounded-lg hover:bg-bg text-text-muted hover:text-text-main transition-colors"
            aria-label="Close test results"
            @click="testResults = null"
          >
            <span class="material-symbols-outlined text-lg">close</span>
          </button>
        </div>
        <div class="p-5">
          <div v-if="testResults.error && !testResults.results" class="text-center py-6">
            <span class="material-symbols-outlined text-red-500 text-[32px] mb-2 block">error</span>
            <p class="text-sm text-red-400">{{ testResults.error }}</p>
          </div>
          <div v-else class="flex min-w-0 flex-col gap-3">
            <div
              v-if="testResults.summary"
              class="flex flex-wrap items-center gap-2 text-xs mb-1 sm:gap-3"
            >
              <span class="text-text-muted">{{ testModeLabel }} Test</span>
              <span class="px-2 py-0.5 rounded bg-emerald-500/15 text-emerald-400 font-medium">
                {{ testResults.summary.passed }} passed
              </span>
              <span
                v-if="testResults.summary.failed > 0"
                class="px-2 py-0.5 rounded bg-red-500/15 text-red-400 font-medium"
              >
                {{ testResults.summary.failed }} failed
              </span>
              <span class="text-text-muted sm:ml-auto">{{ testResults.summary.total }} tested</span>
            </div>
            <div
              v-for="(r, i) in testResults.results || []"
              :key="r.connectionId || i"
              class="flex min-w-0 flex-wrap items-center gap-2 rounded-lg bg-black/3 px-3 py-2 text-xs dark:bg-white/3 sm:flex-nowrap"
            >
              <span
                :class="`material-symbols-outlined text-[16px] ${r.valid ? 'text-emerald-500' : 'text-red-500'}`"
              >
                {{ r.valid ? "check_circle" : "error" }}
              </span>
              <div class="min-w-0 flex-[1_1_160px]">
                <span class="block truncate font-medium sm:inline">{{ r.connectionName }}</span>
                <span class="block truncate text-text-muted sm:ml-1.5 sm:inline">({{ r.provider }})</span>
              </div>
              <span
                v-if="r.latencyMs !== undefined"
                class="shrink-0 text-text-muted font-mono tabular-nums"
              >{{ r.latencyMs }}ms</span>
              <span
                :class="`shrink-0 text-[10px] uppercase font-bold px-1.5 py-0.5 rounded ${
                  r.valid ? 'bg-emerald-500/15 text-emerald-400' : 'bg-red-500/15 text-red-400'
                }`"
              >
                {{ r.valid ? "OK" : r.diagnosis?.type || "ERROR" }}
              </span>
            </div>
            <div
              v-if="(testResults.results || []).length === 0"
              class="text-center py-4 text-text-muted text-sm"
            >
              No active connections found for this group.
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>
