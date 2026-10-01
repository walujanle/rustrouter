<script setup lang="ts">
import { computed, ref } from "vue";
import { useRoute, useRouter } from "vue-router";

import RequestLogger from "@/components/RequestLogger.vue";
import UsageStats from "@/components/UsageStats.vue";
import SegmentedControl from "@/components/ui/SegmentedControl.vue";

const PERIODS = [
	{ value: "today", label: "Today" },
	{ value: "24h", label: "24h" },
	{ value: "7d", label: "7D" },
	{ value: "30d", label: "30D" },
	{ value: "60d", label: "60D" },
	{ value: "all", label: "All" },
];

const TAB_OPTIONS = [
	{ value: "overview", label: "Overview" },
	{ value: "logs", label: "Logs" },
];

const route = useRoute();
const router = useRouter();

const period = ref("today");

function setPeriod(value: string) {
	period.value = value;
}

const activeTab = computed(() => {
	const tab = route.query.tab as string | undefined;
	return tab && ["overview", "logs"].includes(tab) ? tab : "overview";
});

function handleTabChange(value: string) {
	if (value === activeTab.value) return;
	const params = new URLSearchParams(route.query as Record<string, string>);
	params.set("tab", value);
	router.push(`/dashboard/usage?${params.toString()}`);
}
</script>

<template>
  <div class="flex min-w-0 flex-col gap-6 px-1 sm:px-0">
    <!-- Tabs + period selector on same row -->
    <div class="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
      <SegmentedControl
        :options="TAB_OPTIONS"
        :model-value="activeTab"
        class="w-full sm:w-auto"
        @update:model-value="handleTabChange"
      />
      <SegmentedControl
        v-if="activeTab === 'overview'"
        v-model="period"
        :options="PERIODS"
        size="sm"
        class="w-full sm:w-auto"
      />
    </div>

    <UsageStats v-if="activeTab === 'overview'" :period="period" :set-period="setPeriod" hide-period-selector />
    <RequestLogger v-else-if="activeTab === 'logs'" />
  </div>
</template>
