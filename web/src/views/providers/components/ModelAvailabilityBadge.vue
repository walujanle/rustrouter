<script setup lang="ts">
/**
 * ModelAvailabilityBadge — compact inline status indicator
 *
 * Shows green when all models are operational, or amber/red when there are
 * issues, with a hover popover for details and cooldown clearing.
 */

import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import Button from "@/components/ui/UiButton.vue";
import { useNotificationStore } from "@/stores/notification";

const STATUS_CONFIG: Record<string, { icon: string; color: string; label: string }> = {
	available: { icon: "check_circle", color: "#22c55e", label: "Available" },
	cooldown: { icon: "schedule", color: "#f59e0b", label: "Cooldown" },
	unavailable: { icon: "error", color: "#ef4444", label: "Unavailable" },
	unknown: { icon: "help", color: "#6b7280", label: "Unknown" },
};

const data = ref<Record<string, any> | null>(null);
const loading = ref(true);
const expanded = ref(false);
const clearing = ref<string | null>(null);
const containerRef = ref<HTMLElement | null>(null);
const notify = useNotificationStore();

async function fetchStatus() {
	try {
		const res = await fetch("/api/models/availability");
		if (res.ok) {
			data.value = await res.json();
		}
	} catch {
		// silent fail — will retry
	} finally {
		loading.value = false;
	}
}

let interval: ReturnType<typeof setInterval> | null = null;

function handleOutsideClick(e: MouseEvent) {
	if (containerRef.value && !containerRef.value.contains(e.target as Node)) {
		expanded.value = false;
	}
}

onMounted(() => {
	fetchStatus();
	interval = setInterval(fetchStatus, 30000);
});

onBeforeUnmount(() => {
	if (interval) clearInterval(interval);
	document.removeEventListener("mousedown", handleOutsideClick);
});

async function handleClearCooldown(provider: string, model: string) {
	clearing.value = `${provider}:${model}`;
	try {
		const res = await fetch("/api/models/availability", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ action: "clearCooldown", provider, model }),
		});
		if (res.ok) {
			notify.success(`Cooldown cleared for ${model}`);
			await fetchStatus();
		} else {
			notify.error("Failed to clear cooldown");
		}
	} catch {
		notify.error("Failed to clear cooldown");
	} finally {
		clearing.value = null;
	}
}

// The outside-click listener is registered only while the popover is open.
watch(expanded, (open) => {
	if (open) document.addEventListener("mousedown", handleOutsideClick);
	else document.removeEventListener("mousedown", handleOutsideClick);
});

const models = computed<Array<Record<string, any>>>(() => data.value?.models || []);
const unavailableCount = computed(
	() =>
		data.value?.unavailableCount ||
		models.value.filter((m) => m.status !== "available").length,
);
const isHealthy = computed(() => unavailableCount.value === 0);

// Group unhealthy models by provider
const byProvider = computed<Record<string, Array<Record<string, any>>>>(() => {
	const grouped: Record<string, Array<Record<string, any>>> = {};
	for (const m of models.value) {
		if (m.status === "available") continue;
		const key = m.provider || "unknown";
		if (!grouped[key]) grouped[key] = [];
		grouped[key].push(m);
	}
	return grouped;
});
</script>

<template>
  <div v-if="!loading" class="relative" ref="containerRef">
    <!--
      <button
        @click="expanded = !expanded"
        :class="`inline-flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium border transition-all ${
          isHealthy
            ? 'bg-emerald-500/10 border-emerald-500/20 text-emerald-500 hover:bg-emerald-500/15'
            : 'bg-amber-500/10 border-amber-500/20 text-amber-500 hover:bg-amber-500/15'
        }`"
      >
        <span class="material-symbols-outlined text-[14px]">
          {{ isHealthy ? "verified" : "warning" }}
        </span>
        {isHealthy
          ? "All models operational"
          : `${unavailableCount} model${unavailableCount !== 1 ? "s" : ""} with issues`}
      </button>
    -->

    <div
      v-if="expanded"
      class="absolute top-full right-0 mt-2 w-80 bg-surface border border-border rounded-xl shadow-2xl z-50 overflow-hidden"
    >
      <div class="flex items-center justify-between px-4 py-3 border-b border-border bg-bg">
        <div class="flex items-center gap-2">
          <span
            class="material-symbols-outlined text-[16px]"
            :style="{ color: isHealthy ? '#22c55e' : '#f59e0b' }"
          >
            {{ isHealthy ? "verified" : "warning" }}
          </span>
          <span class="text-sm font-semibold text-text-main">Model Status</span>
        </div>
        <button
          type="button"
          @click="fetchStatus"
          class="p-1 rounded-lg hover:bg-surface text-text-muted hover:text-text-main transition-colors"
          title="Refresh"
        >
          <span class="material-symbols-outlined text-[14px]">refresh</span>
        </button>
      </div>

      <div class="px-4 py-3 max-h-60 overflow-y-auto">
        <p v-if="isHealthy" class="text-sm text-text-muted text-center py-2">
          All models are responding normally.
        </p>
        <div v-else class="flex flex-col gap-2.5">
          <div v-for="(provModels, provider) in byProvider" :key="provider">
            <p class="text-xs font-semibold text-text-main mb-1.5 capitalize">{{ provider }}</p>
            <div class="flex flex-col gap-1">
              <div
                v-for="m in provModels"
                :key="`${m.provider}-${m.model}`"
                class="flex items-center justify-between px-2.5 py-1.5 rounded-lg bg-surface/30"
              >
                <div class="flex items-center gap-1.5 min-w-0">
                  <span
                    class="material-symbols-outlined text-[14px] shrink-0"
                    :style="{ color: (STATUS_CONFIG[m.status] || STATUS_CONFIG.unknown).color }"
                  >
                    {{ (STATUS_CONFIG[m.status] || STATUS_CONFIG.unknown).icon }}
                  </span>
                  <span class="font-mono text-xs text-text-main truncate">{{ m.model }}</span>
                </div>
                <Button
                  v-if="m.status === 'cooldown'"
                  size="sm"
                  variant="ghost"
                  :disabled="clearing === `${m.provider}:${m.model}`"
                  class="text-[10px] px-1.5! py-0.5! ml-2"
                  @click="handleClearCooldown(m.provider, m.model)"
                >
                  {{ clearing === `${m.provider}:${m.model}` ? "..." : "Clear" }}
                </Button>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>
