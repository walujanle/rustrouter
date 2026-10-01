<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";

import Card from "@/components/ui/UiCard.vue";

const logs = ref<string[]>([]);
const loading = ref(true);
const autoRefresh = ref(true);
let interval: ReturnType<typeof setInterval> | null = null;

async function fetchLogs(showLoading = true) {
	if (showLoading) loading.value = true;
	try {
		const res = await fetch("/api/usage/request-logs");
		if (res.ok) logs.value = await res.json();
	} catch (error) {
		console.error("Failed to fetch logs:", error);
	} finally {
		if (showLoading) loading.value = false;
	}
}

onMounted(() => {
	fetchLogs();
	interval = setInterval(() => {
		if (autoRefresh.value) fetchLogs(false);
	}, 3000);
});

onBeforeUnmount(() => {
	if (interval) clearInterval(interval);
});

interface ParsedLog {
	key: number;
	parts: string[];
	status: string;
	isPending: boolean;
	isFailed: boolean;
	isSuccess: boolean;
}

function parseLogs(): ParsedLog[] {
	const out: ParsedLog[] = [];
	logs.value.forEach((log, i) => {
		const parts = log.split(" | ");
		if (parts.length < 7) return;
		const status = parts[6];
		out.push({
			key: i,
			parts,
			status,
			isPending: status.includes("PENDING"),
			isFailed: status.includes("FAILED"),
			isSuccess: status.includes("OK"),
		});
	});
	return out;
}
</script>

<template>
  <div class="flex flex-col gap-4">
    <div class="flex items-center justify-between">
      <h2 class="text-xl font-semibold">Request Logs</h2>
      <div class="flex items-center gap-2">
        <span class="text-sm font-medium text-text-muted flex items-center gap-2">
          <span>Auto Refresh (3s)</span>
          <button
            type="button"
            role="switch"
            :aria-checked="autoRefresh"
            aria-label="Auto refresh request logs"
            :class="`relative inline-flex h-5 w-9 items-center rounded-full transition-colors ${autoRefresh ? 'bg-primary' : 'bg-surface-2 border border-border'}`"
            @click="autoRefresh = !autoRefresh"
          >
            <span
              :class="`inline-block h-3 w-3 transform rounded-full bg-white transition-transform ${autoRefresh ? 'translate-x-5' : 'translate-x-1'}`"
            />
          </button>
        </span>
      </div>
    </div>

    <Card class="overflow-hidden bg-black/5 dark:bg-black/20">
      <div class="p-0 overflow-x-auto max-h-150 overflow-y-auto font-mono text-xs">
        <div v-if="loading && logs.length === 0" class="p-8 text-center text-text-muted">Loading logs...</div>
        <div v-else-if="logs.length === 0" class="p-8 text-center text-text-muted">No logs recorded yet.</div>
        <table v-else class="w-full text-left border-collapse whitespace-nowrap">
          <thead class="sticky top-0 bg-surface-2 border-b border-border z-10">
            <tr>
              <th class="px-3 py-2 border-r border-border">DateTime</th>
              <th class="px-3 py-2 border-r border-border">Model</th>
              <th class="px-3 py-2 border-r border-border">Provider</th>
              <th class="px-3 py-2 border-r border-border">Account</th>
              <th class="px-3 py-2 border-r border-border">In</th>
              <th class="px-3 py-2 border-r border-border">Out</th>
              <th class="px-3 py-2">Status</th>
            </tr>
          </thead>
          <tbody class="divide-y divide-border/50">
            <tr
              v-for="log in parseLogs()"
              :key="log.key"
              :class="`hover:bg-primary/5 transition-colors ${log.isPending ? 'bg-primary/5' : ''}`"
            >
              <td class="px-3 py-1.5 border-r border-border text-text-muted">{{ log.parts[0] }}</td>
              <td class="px-3 py-1.5 border-r border-border font-medium">{{ log.parts[1] }}</td>
              <td class="px-3 py-1.5 border-r border-border">
                <span class="px-1.5 py-0.5 rounded bg-surface-2 border border-border text-[10px] uppercase font-bold">
                  {{ log.parts[2] }}
                </span>
              </td>
              <td class="px-3 py-1.5 border-r border-border truncate max-w-37.5" :title="log.parts[3]">{{ log.parts[3] }}</td>
              <td class="px-3 py-1.5 border-r border-border text-right text-primary">{{ log.parts[4] }}</td>
              <td class="px-3 py-1.5 border-r border-border text-right text-success">{{ log.parts[5] }}</td>
              <td
                :class="`px-3 py-1.5 font-bold ${log.isSuccess ? 'text-success' : log.isFailed ? 'text-error' : 'text-primary animate-pulse'}`"
              >
                {{ log.status }}
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </Card>
    <div class="text-[10px] text-text-muted italic">
      Logs are loaded from the request history database.
    </div>
  </div>
</template>
