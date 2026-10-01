<script setup lang="ts">
import { computed, defineComponent, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";

import Badge from "@/components/ui/UiBadge.vue";
import Card from "@/components/ui/UiCard.vue";
import { useProviders } from "@/constants/providers";
import OverviewCards from "@/views/usage/components/OverviewCards.vue";
import ProviderBarChart from "@/views/usage/components/ProviderBarChart.vue";
import ProviderTopology from "@/views/usage/components/ProviderTopology.vue";
import TopModelsChart from "@/views/usage/components/TopModelsChart.vue";
import UsageChart from "@/views/usage/components/UsageChart.vue";
import UsageTable, { fmt, fmtTime } from "@/views/usage/components/UsageTable.vue";

function timeAgo(timestamp: string | number): string {
	const diff = Math.floor((Date.now() - new Date(timestamp).getTime()) / 1000);
	if (diff < 60) return `${diff}s ago`;
	if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
	if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
	return `${Math.floor(diff / 86400)}d ago`;
}

// Auto-update time display every second without re-rendering parent
const TimeAgo = defineComponent({
	name: "TimeAgo",
	props: { timestamp: { type: [String, Number], required: true } },
	setup(taProps) {
		const tick = ref(0);
		let timer: ReturnType<typeof setInterval> | null = null;
		onMounted(() => {
			timer = setInterval(() => {
				tick.value += 1;
			}, 1000);
		});
		onBeforeUnmount(() => {
			if (timer) clearInterval(timer);
		});
		return () => {
			void tick.value;
			return timeAgo(taProps.timestamp);
		};
	},
});

const props = withDefaults(
	defineProps<{ period?: string; setPeriod?: (value: string) => void; hidePeriodSelector?: boolean }>(),
	{ hidePeriodSelector: false },
);

const router = useRouter();
const route = useRoute();
const { AI_PROVIDERS, FREE_PROVIDERS } = useProviders();

const sortBy = computed(() => (route.query.sortBy as string) || "rawModel");
const sortOrder = computed(() => (route.query.sortOrder as string) || "asc");

const stats = ref<Record<string, any> | null>(null);
const loading = ref(true);
const fetching = ref(false);
const tableView = ref("model");
const viewMode = ref("costs");
const providers = ref<Array<Record<string, any>>>([]);
const periodLocal = ref("today");
let isInitialLoad = true;
let hasLoadedStats = false;

const period = computed(() => props.period ?? periodLocal.value);
function setPeriod(value: string) {
	if (props.setPeriod) props.setPeriod(value);
	else periodLocal.value = value;
}

// Keep providers without serviceKinds (default LLM) or with "llm" in serviceKinds
function isLLMProvider(id: string) {
	const p = AI_PROVIDERS[id];
	if (!p?.serviceKinds) return true;
	return p.serviceKinds.includes("llm");
}

const MODEL_COLUMNS = [
	{ field: "rawModel", label: "Model" },
	{ field: "provider", label: "Provider" },
	{ field: "requests", label: "Requests", align: "right" },
	{ field: "lastUsed", label: "Last Used", align: "right" },
];
const ACCOUNT_COLUMNS = [
	{ field: "rawModel", label: "Model" },
	{ field: "provider", label: "Provider" },
	{ field: "accountName", label: "Account" },
	{ field: "requests", label: "Requests", align: "right" },
	{ field: "lastUsed", label: "Last Used", align: "right" },
];
const API_KEY_COLUMNS = [
	{ field: "keyName", label: "API Key Name" },
	{ field: "rawModel", label: "Model" },
	{ field: "provider", label: "Provider" },
	{ field: "requests", label: "Requests", align: "right" },
	{ field: "lastUsed", label: "Last Used", align: "right" },
];
const ENDPOINT_COLUMNS = [
	{ field: "endpoint", label: "Endpoint" },
	{ field: "rawModel", label: "Model" },
	{ field: "provider", label: "Provider" },
	{ field: "requests", label: "Requests", align: "right" },
	{ field: "lastUsed", label: "Last Used", align: "right" },
];
const TABLE_OPTIONS = [
	{ value: "model", label: "Usage by Model" },
	{ value: "account", label: "Usage by Account" },
	{ value: "apiKey", label: "Usage by API Key" },
	{ value: "endpoint", label: "Usage by Endpoint" },
];
const PERIODS = [
	{ value: "today", label: "Today" },
	{ value: "24h", label: "24h" },
	{ value: "7d", label: "7D" },
	{ value: "30d", label: "30D" },
	{ value: "60d", label: "60D" },
	{ value: "all", label: "All" },
];

function sortData(dataMap: Record<string, any>, pendingMap: Record<string, number> = {}, by: string, order: string) {
	return Object.entries(dataMap || {})
		.map(([key, data]: [string, any]) => {
			const totalTokens = (data.promptTokens || 0) + (data.completionTokens || 0);
			const totalCost = data.cost || 0;
			// ponytail: cost split is a token-share allocation of the (rate-accurate)
			// server total, not a per-rate recompute. cached is a subset of prompt, so
			// peel it out of the input share. Upgrade to a stored per-component cost
			// breakdown if exact cached-rate cost display is needed.
			const cachedTokens = data.cachedTokens || 0;
			const nonCachedInput = Math.max(0, (data.promptTokens || 0) - cachedTokens);
			const inputCost = totalTokens > 0 ? nonCachedInput * (totalCost / totalTokens) : 0;
			const cachedCost = totalTokens > 0 ? cachedTokens * (totalCost / totalTokens) : 0;
			const outputCost = totalTokens > 0 ? (data.completionTokens || 0) * (totalCost / totalTokens) : 0;
			return { ...data, key, totalTokens, totalCost, inputCost, cachedCost, outputCost, pending: pendingMap[key] || 0 };
		})
		.sort((a: any, b: any) => {
			let valA = a[by];
			let valB = b[by];
			if (typeof valA === "string") valA = valA.toLowerCase();
			if (typeof valB === "string") valB = valB.toLowerCase();
			if (valA < valB) return order === "asc" ? -1 : 1;
			if (valA > valB) return order === "asc" ? 1 : -1;
			return 0;
		});
}

function getGroupKey(item: any, keyField: string) {
	switch (keyField) {
		case "rawModel":
			return item.rawModel || "Unknown Model";
		case "accountName":
			return item.accountName || `Account ${item.connectionId?.slice(0, 8)}...` || "Unknown Account";
		case "keyName":
			return item.keyName || "Unknown Key";
		case "endpoint":
			return item.endpoint || "Unknown Endpoint";
		default:
			return item[keyField] || "Unknown";
	}
}

function groupDataByKey(data: any, keyField: string) {
	if (!Array.isArray(data)) return [];
	const groups: Record<string, any> = {};
	data.forEach((item) => {
		const gk = getGroupKey(item, keyField);
		if (!groups[gk]) {
			groups[gk] = {
				groupKey: gk,
				summary: {
					requests: 0,
					promptTokens: 0,
					completionTokens: 0,
					cachedTokens: 0,
					totalTokens: 0,
					cost: 0,
					inputCost: 0,
					cachedCost: 0,
					outputCost: 0,
					lastUsed: null,
					pending: 0,
				},
				items: [],
			};
		}
		const s = groups[gk].summary;
		s.requests += item.requests || 0;
		s.promptTokens += item.promptTokens || 0;
		s.completionTokens += item.completionTokens || 0;
		s.cachedTokens += item.cachedTokens || 0;
		s.totalTokens += item.totalTokens || 0;
		s.cost += item.cost || 0;
		s.inputCost += item.inputCost || 0;
		s.cachedCost += item.cachedCost || 0;
		s.outputCost += item.outputCost || 0;
		s.pending += item.pending || 0;
		if (item.lastUsed && (!s.lastUsed || new Date(item.lastUsed) > new Date(s.lastUsed))) {
			s.lastUsed = item.lastUsed;
		}
		groups[gk].items.push(item);
	});
	return Object.values(groups);
}

onMounted(() => {
	Promise.all([
		fetch("/api/providers").then((r) => (r.ok ? r.json() : null)),
		fetch("/api/provider-nodes").then((r) => (r.ok ? r.json() : null)),
	])
		.then(([d, nodesData]) => {
			const nodeNameMap: Record<string, string> = {};
			for (const node of nodesData?.nodes || []) {
				nodeNameMap[node.id] = node.name;
			}
			const seen = new Set<string>();
			const unique = (d?.connections || [])
				.filter((c: any) => {
					if (c.isActive === false) return false;
					if (!isLLMProvider(c.provider)) return false;
					if (seen.has(c.provider)) return false;
					seen.add(c.provider);
					return true;
				})
				.map((c: any) => ({ ...c, nodeName: nodeNameMap[c.provider] || null }));
			const noAuthProviders = Object.values(FREE_PROVIDERS)
				.filter((p: any) => p.noAuth && !seen.has(p.id) && isLLMProvider(p.id))
				.map((p: any) => ({ provider: p.id, name: p.name }));
			providers.value = [...unique, ...noAuthProviders];
		})
		.catch(() => {});
});

async function fetchStats() {
	if (isInitialLoad) {
		isInitialLoad = false;
		loading.value = true;
	} else {
		fetching.value = true;
	}
	try {
		const r = await fetch(`/api/usage/stats?period=${period.value}`);
		const data = r.ok ? await r.json() : null;
		if (data) {
			hasLoadedStats = true;
			stats.value = { ...(stats.value || {}), ...data };
		}
	} catch {
		/* keep last stats */
	} finally {
		loading.value = false;
		fetching.value = false;
	}
}

// SSE connection - real-time updates for activeRequests + recentRequests only
let eventSource: EventSource | null = null;

function connectStream() {
	eventSource = new EventSource("/api/usage/stream");
	eventSource.onmessage = (e) => {
		try {
			const data = JSON.parse(e.data);
			if (!stats.value) return;
			stats.value = {
				...stats.value,
				activeRequests: data.activeRequests,
				recentRequests: data.recentRequests,
				errorProvider: data.errorProvider,
				pending: data.pending,
			};
			if (hasLoadedStats) loading.value = false;
		} catch (err) {
			console.error("[SSE CLIENT] parse error:", err);
		}
	};
	eventSource.onerror = () => {
		loading.value = false;
	};
}

onMounted(() => {
	fetchStats();
	connectStream();
});

// Refetch when the period changes.
watch(period, fetchStats);

onBeforeUnmount(() => {
	eventSource?.close();
});

function toggleSort(_tableType: string, field: string) {
	const params = new URLSearchParams(route.query as Record<string, string>);
	if (params.get("sortBy") === field) {
		params.set("sortOrder", params.get("sortOrder") === "asc" ? "desc" : "asc");
	} else {
		params.set("sortBy", field);
		params.set("sortOrder", "asc");
	}
	router.replace(`?${params.toString()}`);
}

const activeTableConfig = computed(() => {
	const s = stats.value;
	if (!s) return null;
	switch (tableView.value) {
		case "model": {
			const pendingMap = s.pending?.byModel || {};
			return {
				columns: MODEL_COLUMNS,
				groupedData: groupDataByKey(sortData(s.byModel, pendingMap, sortBy.value, sortOrder.value), "rawModel"),
				storageKey: "usage-stats:expanded-models",
				emptyMessage: "No usage recorded yet.",
			};
		}
		case "account": {
			const pendingMap: Record<string, number> = {};
			if (s?.pending?.byAccount) {
				Object.entries(s.byAccount || {}).forEach(([accountKey, data]: [string, any]) => {
					const connPending = s.pending.byAccount[data.connectionId];
					if (connPending) {
						const modelKey = data.provider ? `${data.rawModel} (${data.provider})` : data.rawModel;
						pendingMap[accountKey] = connPending[modelKey] || 0;
					}
				});
			}
			return {
				columns: ACCOUNT_COLUMNS,
				groupedData: groupDataByKey(sortData(s.byAccount, pendingMap, sortBy.value, sortOrder.value), "accountName"),
				storageKey: "usage-stats:expanded-accounts",
				emptyMessage: "No account-specific usage recorded yet.",
			};
		}
		case "apiKey":
			return {
				columns: API_KEY_COLUMNS,
				groupedData: groupDataByKey(sortData(s.byApiKey, {}, sortBy.value, sortOrder.value), "keyName"),
				storageKey: "usage-stats:expanded-apikeys",
				emptyMessage: "No API key usage recorded yet.",
			};
		default:
			return {
				columns: ENDPOINT_COLUMNS,
				groupedData: groupDataByKey(sortData(s.byEndpoint, {}, sortBy.value, sortOrder.value), "endpoint"),
				storageKey: "usage-stats:expanded-endpoints",
				emptyMessage: "No endpoint usage recorded yet.",
			};
	}
});
</script>

<template>
  <div v-if="!stats && !loading" class="text-text-muted">Failed to load usage statistics.</div>

  <div v-else class="flex min-w-0 flex-col gap-6">
    <!-- Period selector (hidden when controlled by parent) -->
    <div v-if="!props.hidePeriodSelector" class="flex w-full items-center gap-2 sm:w-auto sm:self-end">
      <div class="grid flex-1 grid-cols-6 items-center gap-1 rounded-lg border border-border bg-surface-2 p-1 sm:flex sm:flex-none">
        <button
          v-for="p in PERIODS"
          :key="p.value"
          type="button"
          :disabled="fetching"
          :class="`rounded-md px-3 py-1 text-sm font-medium transition-colors ${period === p.value ? 'bg-primary text-white shadow-sm' : 'text-text-muted hover:bg-surface-3 hover:text-text'}`"
          @click="setPeriod(p.value)"
        >
          {{ p.label }}
        </button>
      </div>
      <span v-if="fetching" class="material-symbols-outlined text-[16px] text-text-muted animate-spin">progress_activity</span>
    </div>

    <!-- Overview cards -->
    <div v-if="loading" class="flex items-center justify-center py-12 text-text-muted">
      <span class="material-symbols-outlined text-[32px] animate-spin">progress_activity</span>
    </div>
    <OverviewCards v-else-if="stats" :stats="stats" />

    <!-- Provider topology + Recent Requests -->
    <div v-if="loading" class="flex items-center justify-center py-12 text-text-muted">
      <span class="material-symbols-outlined text-[32px] animate-spin">progress_activity</span>
    </div>
    <div v-else-if="stats" class="grid min-w-0 grid-cols-1 items-stretch gap-2 lg:grid-cols-[minmax(0,2fr)_minmax(280px,1fr)]">
      <ProviderTopology
        :providers="providers"
        :active-requests="stats.activeRequests || []"
        :last-provider="stats.recentRequests?.[0]?.provider || ''"
        :error-provider="stats.errorProvider || ''"
      />
      <Card class="flex min-w-0 flex-col overflow-hidden" padding="sm" :style="{ height: '480px' }">
        <div class="px-1 py-2 border-b border-border shrink-0">
          <span class="text-xs font-semibold text-text-muted uppercase tracking-wide">Recent Requests</span>
        </div>

        <div v-if="!(stats.recentRequests || []).length" class="flex-1 flex items-center justify-center text-text-muted text-sm">
          No requests yet.
        </div>
        <div v-else class="flex-1 overflow-y-auto">
          <table class="w-full min-w-75 border-collapse text-xs">
            <thead class="sticky top-0 bg-bg z-10">
              <tr class="border-b border-border">
                <th class="py-1.5 text-left font-semibold text-text-muted w-2" />
                <th class="py-1.5 text-left font-semibold text-text-muted">Model</th>
                <th class="py-1.5 text-right font-semibold text-text-muted whitespace-nowrap">In / Out</th>
                <th class="py-1.5 text-right font-semibold text-text-muted">When</th>
              </tr>
            </thead>
            <tbody class="divide-y divide-border/50">
              <tr
                v-for="(r, i) in stats.recentRequests || []"
                :key="i"
                class="hover:bg-surface-2 transition-colors"
              >
                <td class="py-1.5">
                  <span :class="`block w-1.5 h-1.5 rounded-full ${!r.status || r.status === 'ok' || r.status === 'success' ? 'bg-success' : 'bg-error'}`" />
                </td>
                <td class="py-1.5 font-mono truncate max-w-30" :title="r.model">{{ r.model }}</td>
                <td class="py-1.5 text-right whitespace-nowrap">
                  <span class="text-primary">{{ fmt(r.promptTokens) }}↑</span>
                  {{ " " }}
                  <span class="text-success">{{ fmt(r.completionTokens) }}↓</span>
                </td>
                <td class="py-1.5 text-right text-text-muted whitespace-nowrap">
                  <TimeAgo :timestamp="r.timestamp" />
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </Card>
    </div>

    <!-- Token / Cost chart - sync period -->
    <div v-if="loading" class="flex items-center justify-center py-12 text-text-muted">
      <span class="material-symbols-outlined text-[32px] animate-spin">progress_activity</span>
    </div>
    <UsageChart v-else :period="period" />

    <!-- Provider and model breakdown charts -->
    <div v-if="!loading && stats && (stats.byProvider || stats.byModel)" class="grid min-w-0 grid-cols-1 gap-2 lg:grid-cols-2">
      <ProviderBarChart :by-provider="stats.byProvider" />
      <TopModelsChart :by-model="stats.byModel" />
    </div>

    <!-- Table with dropdown selector -->
    <div class="flex flex-col gap-3">
      <div class="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
        <select
          v-model="tableView"
          class="w-full rounded-lg border border-border bg-surface px-3 py-1.5 text-sm font-medium text-text-main focus:outline-none focus:ring-2 focus:ring-primary/50 sm:w-auto"
          style="color-scheme: auto"
        >
          <option v-for="opt in TABLE_OPTIONS" :key="opt.value" :value="opt.value">{{ opt.label }}</option>
        </select>
        <div class="grid grid-cols-2 items-center gap-1 rounded-lg border border-border bg-surface-2 p-1 sm:flex">
          <button
            type="button"
            :class="`px-3 py-1 rounded-md text-sm font-medium transition-colors ${viewMode === 'costs' ? 'bg-primary text-white shadow-sm' : 'text-text-muted hover:text-text hover:bg-surface-3'}`"
            @click="viewMode = 'costs'"
          >
            Costs
          </button>
          <button
            type="button"
            :class="`px-3 py-1 rounded-md text-sm font-medium transition-colors ${viewMode === 'tokens' ? 'bg-primary text-white shadow-sm' : 'text-text-muted hover:text-text hover:bg-surface-3'}`"
            @click="viewMode = 'tokens'"
          >
            Tokens
          </button>
        </div>
      </div>
      <div v-if="loading" class="flex items-center justify-center py-12 text-text-muted">
        <span class="material-symbols-outlined text-[32px] animate-spin">progress_activity</span>
      </div>
      <UsageTable
        v-else-if="activeTableConfig"
        title=""
        :columns="activeTableConfig.columns"
        :grouped-data="activeTableConfig.groupedData"
        :table-type="tableView"
        :sort-by="sortBy"
        :sort-order="sortOrder"
        :view-mode="viewMode"
        :storage-key="activeTableConfig.storageKey"
        :empty-message="activeTableConfig.emptyMessage"
        @toggle-sort="toggleSort"
      >
        <template #summary-cells="{ group }">
          <template v-if="tableView === 'model'">
            <td class="px-6 py-3 text-text-muted">—</td>
            <td class="px-6 py-3 text-right">{{ fmt(group.summary.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(group.summary.lastUsed) }}</td>
          </template>
          <template v-else-if="tableView === 'account'">
            <td class="px-6 py-3 text-text-muted">—</td>
            <td class="px-6 py-3 text-text-muted">—</td>
            <td class="px-6 py-3 text-right">{{ fmt(group.summary.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(group.summary.lastUsed) }}</td>
          </template>
          <template v-else-if="tableView === 'apiKey'">
            <td class="px-6 py-3 text-text-muted">—</td>
            <td class="px-6 py-3 text-text-muted">—</td>
            <td class="px-6 py-3 text-right">{{ fmt(group.summary.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(group.summary.lastUsed) }}</td>
          </template>
          <template v-else>
            <td class="px-6 py-3 text-text-muted">—</td>
            <td class="px-6 py-3 text-text-muted">—</td>
            <td class="px-6 py-3 text-right">{{ fmt(group.summary.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(group.summary.lastUsed) }}</td>
          </template>
        </template>

        <template #detail-cells="{ item }">
          <template v-if="tableView === 'model'">
            <td :class="`px-6 py-3 font-medium transition-colors ${item.pending > 0 ? 'text-primary' : ''}`">{{ item.rawModel }}</td>
            <td class="px-6 py-3"><Badge :variant="item.pending > 0 ? 'primary' : 'default'" size="sm">{{ item.provider }}</Badge></td>
            <td class="px-6 py-3 text-right">{{ fmt(item.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(item.lastUsed) }}</td>
          </template>
          <template v-else-if="tableView === 'account'">
            <td :class="`px-6 py-3 font-medium transition-colors ${item.pending > 0 ? 'text-primary' : ''}`">
              {{ item.accountName || `Account ${item.connectionId?.slice(0, 8)}...` }}
            </td>
            <td :class="`px-6 py-3 font-medium transition-colors ${item.pending > 0 ? 'text-primary' : ''}`">{{ item.rawModel }}</td>
            <td class="px-6 py-3"><Badge :variant="item.pending > 0 ? 'primary' : 'default'" size="sm">{{ item.provider }}</Badge></td>
            <td class="px-6 py-3 text-right">{{ fmt(item.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(item.lastUsed) }}</td>
          </template>
          <template v-else-if="tableView === 'apiKey'">
            <td class="px-6 py-3 font-medium">{{ item.keyName }}</td>
            <td class="px-6 py-3">{{ item.rawModel }}</td>
            <td class="px-6 py-3"><Badge variant="default" size="sm">{{ item.provider }}</Badge></td>
            <td class="px-6 py-3 text-right">{{ fmt(item.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(item.lastUsed) }}</td>
          </template>
          <template v-else>
            <td class="px-6 py-3 font-medium font-mono text-sm">{{ item.endpoint }}</td>
            <td class="px-6 py-3">{{ item.rawModel }}</td>
            <td class="px-6 py-3"><Badge variant="default" size="sm">{{ item.provider }}</Badge></td>
            <td class="px-6 py-3 text-right">{{ fmt(item.requests) }}</td>
            <td class="px-6 py-3 text-right text-text-muted whitespace-nowrap">{{ fmtTime(item.lastUsed) }}</td>
          </template>
        </template>
      </UsageTable>
    </div>
  </div>
</template>
