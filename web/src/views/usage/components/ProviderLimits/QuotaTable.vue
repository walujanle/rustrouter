<script setup lang="ts">
import { computed, ref, watch } from "vue";
import type { NormalizedQuota, QuotaRow } from "./utils";
import { formatResetTime, getRemainingPercentage } from "./utils";

const PAGE_SIZE = 10;

const props = withDefaults(
	defineProps<{
		quotas?: QuotaRow[];
		compact?: boolean;
		sortMode?: string;
		showSortLabel?: boolean;
		onHideQuota?: ((quota: NormalizedQuota) => void) | null;
	}>(),
	{
		quotas: () => [],
		compact: false,
		sortMode: "default",
		showSortLabel: false,
		onHideQuota: null,
	},
);

/**
 * Format reset time display (Today, 12:00 PM)
 */
function formatResetTimeDisplay(resetTime: string | null | undefined) {
	if (!resetTime) return null;

	try {
		const date = new Date(resetTime);
		const now = new Date();
		const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
		const tomorrow = new Date(today);
		tomorrow.setDate(tomorrow.getDate() + 1);

		let dayStr = "";
		if (date >= today && date < tomorrow) {
			dayStr = "Today";
		} else if (
			date >= tomorrow &&
			date < new Date(tomorrow.getTime() + 24 * 60 * 60 * 1000)
		) {
			dayStr = "Tomorrow";
		} else {
			dayStr = date.toLocaleDateString("en-US", {
				month: "short",
				day: "numeric",
			});
		}

		const timeStr = date.toLocaleTimeString("en-US", {
			hour: "numeric",
			minute: "2-digit",
			hour12: true,
		});

		return `${dayStr}, ${timeStr}`;
	} catch {
		return null;
	}
}

/**
 * Get color classes based on remaining percentage
 */
function getColorClasses(remainingPercentage: number) {
	if (remainingPercentage > 70) {
		return {
			text: "text-green-600 dark:text-green-400",
			bg: "bg-green-500",
			bgLight: "bg-green-500/10",
			emoji: "🟢",
		};
	}

	if (remainingPercentage >= 30) {
		return {
			text: "text-yellow-600 dark:text-yellow-400",
			bg: "bg-yellow-500",
			bgLight: "bg-yellow-500/10",
			emoji: "🟡",
		};
	}

	return {
		text: "text-red-600 dark:text-red-400",
		bg: "bg-red-500",
		bgLight: "bg-red-500/10",
		emoji: "🔴",
	};
}

function sortQuotas(quotas: NormalizedQuota[], sortMode: string) {
	if (sortMode === "remaining-asc") {
		return [...quotas].sort(
			(a, b) => a.remaining - b.remaining || a.name.localeCompare(b.name),
		);
	}

	if (sortMode === "remaining-desc") {
		return [...quotas].sort(
			(a, b) => b.remaining - a.remaining || a.name.localeCompare(b.name),
		);
	}

	return quotas;
}

const page = ref(1);

const normalizedQuotas = computed<NormalizedQuota[]>(() =>
	props.quotas.map((quota, index) => ({
		...quota,
		index,
		remaining: getRemainingPercentage(quota),
	})),
);

const sortedQuotas = computed(() =>
	sortQuotas(normalizedQuotas.value, props.sortMode),
);

const totalPages = computed(() =>
	Math.max(1, Math.ceil(sortedQuotas.value.length / PAGE_SIZE)),
);

watch([() => props.sortMode, () => props.quotas], () => {
	page.value = 1;
});

watch(totalPages, (value) => {
	page.value = Math.min(page.value, value);
});

const currentPageRows = computed(() =>
	sortedQuotas.value.slice((page.value - 1) * PAGE_SIZE, page.value * PAGE_SIZE),
);
const pageStart = computed(() =>
	sortedQuotas.value.length === 0 ? 0 : (page.value - 1) * PAGE_SIZE + 1,
);
const pageEnd = computed(() =>
	Math.min(page.value * PAGE_SIZE, sortedQuotas.value.length),
);

const cellPad = computed(() => (props.compact ? "py-1 px-1.5" : "py-2 px-3"));
const nameText = computed(() => (props.compact ? "text-[11px]" : "text-sm"));
const resetPrimary = computed(() => (props.compact ? "text-[11px]" : "text-sm"));
const resetSecondary = computed(() =>
	props.compact ? "text-[10px] leading-tight" : "text-xs",
);
const sortLabel = "Sorted by account remaining";
const hasHideAction = computed(() => typeof props.onHideQuota === "function");
</script>

<template>
  <div v-if="quotas && quotas.length > 0" class="space-y-2">
    <div class="flex items-center justify-between gap-2">
      <div class="text-[10px] text-text-muted">
        {{ sortedQuotas.length }} quota{{ sortedQuotas.length > 1 ? "s" : "" }}
      </div>
      <div
        v-if="props.showSortLabel"
        class="rounded-md border border-black/10 bg-black/2 px-2 py-1 text-[10px] text-text-muted dark:border-white/10 dark:bg-white/3"
      >
        {{ sortLabel }}
      </div>
    </div>

    <div class="space-y-px">
      <div
        v-for="quota in currentPageRows"
        :key="`${quota.name}-${quota.index}`"
        :class="`flex items-center gap-2 border-b border-black/5 dark:border-white/5 hover:bg-black/2 dark:hover:bg-white/2 transition-colors ${cellPad}`"
      >
        <!-- Name -->
        <div class="flex w-36 min-w-0 items-center gap-1.5">
          <span class="text-[10px] shrink-0">{{ getColorClasses(quota.remaining).emoji }}</span>
          <span :class="`${nameText} font-medium text-text-primary truncate`">
            {{ quota.name }}
          </span>
        </div>

        <!-- Progress + used/total -->
        <div :class="`min-w-0 flex-1 ${props.compact ? 'space-y-1' : 'space-y-1.5'}`">
          <div
            v-if="quota.unlimited !== true && quota.isCreditBalance !== true"
            :class="`${props.compact ? 'h-1' : 'h-1.5'} rounded-full overflow-hidden border ${getColorClasses(quota.remaining).bgLight} ${
              quota.remaining === 0 ? 'border-black/10 dark:border-white/10' : 'border-transparent'
            }`"
          >
            <div
              :class="`h-full transition-all duration-300 ${getColorClasses(quota.remaining).bg}`"
              :style="{ width: `${Math.min(quota.remaining, 100)}%` }"
            />
          </div>

          <div
            :class="`flex items-center justify-between gap-1 min-w-0 ${props.compact ? 'text-[10px]' : 'text-xs'}`"
          >
            <span
              class="text-text-muted truncate"
              :title="
                quota.unlimited === true
                  ? `${quota.used.toLocaleString()} used · Unlimited`
                  : quota.isCreditBalance === true
                    ? `Credit balance: ${quota.total.toFixed(2)} ${quota.currency || ''}`
                    : `${quota.used.toLocaleString()} / ${quota.total > 0 ? quota.total.toLocaleString() : '∞'}`
              "
            >
              {{
                quota.unlimited === true
                  ? `${quota.used.toLocaleString()} used · Unlimited`
                  : quota.isCreditBalance === true
                    ? `Credit: ${quota.total.toFixed(2)} ${quota.currency || ''}`
                    : `${quota.used.toLocaleString()} / ${quota.total > 0 ? quota.total.toLocaleString() : '∞'}`
              }}
            </span>
            <span
              :class="`font-medium ${quota.unlimited === true ? 'text-green-600 dark:text-green-400' : quota.isCreditBalance === true ? 'text-blue-600 dark:text-blue-400' : getColorClasses(quota.remaining).text} shrink-0`"
            >
              {{
                quota.unlimited === true
                  ? "Unlimited"
                  : quota.isCreditBalance === true
                    ? ""
                    : `${quota.remaining}%`
              }}
            </span>
          </div>
        </div>

        <!-- Reset time -->
        <div class="min-w-0 shrink">
          <template v-if="formatResetTime(quota.resetAt) !== '-' || formatResetTimeDisplay(quota.resetAt)">
            <template v-if="props.compact">
              <div
                :class="`${resetPrimary} text-text-primary font-medium truncate`"
                :title="formatResetTimeDisplay(quota.resetAt) || ''"
              >
                {{
                  formatResetTime(quota.resetAt) !== "-"
                    ? quota.recurring !== false
                      ? `in ${formatResetTime(quota.resetAt)}`
                      : `expires in ${formatResetTime(quota.resetAt)}`
                    : formatResetTimeDisplay(quota.resetAt)
                }}
              </div>
            </template>
            <template v-else>
              <div class="min-w-0 space-y-0.5">
                <div
                  v-if="formatResetTime(quota.resetAt) !== '-'"
                  :class="`${resetPrimary} text-text-primary font-medium truncate`"
                >
                  {{
                    quota.recurring !== false
                      ? `in ${formatResetTime(quota.resetAt)}`
                      : `expires in ${formatResetTime(quota.resetAt)}`
                  }}
                </div>
                <div
                  v-if="formatResetTimeDisplay(quota.resetAt)"
                  :class="`${resetSecondary} text-text-muted truncate`"
                >
                  {{ formatResetTimeDisplay(quota.resetAt) }}
                </div>
              </div>
            </template>
          </template>
          <div v-else :class="`${resetPrimary} text-text-muted italic`">N/A</div>
        </div>

        <!-- Hide action -->
        <button
          v-if="hasHideAction"
          type="button"
          class="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-text-muted transition-colors hover:bg-black/5 hover:text-text-primary dark:hover:bg-white/5"
          title="Hide this quota row"
          :aria-label="`Hide quota ${quota.name}`"
          @click="props.onHideQuota?.(quota)"
        >
          <span class="material-symbols-outlined text-[15px]"> visibility_off </span>
        </button>
      </div>
    </div>

    <div
      v-if="totalPages > 1"
      class="rounded-md border border-black/10 bg-black/2 px-2 py-1.5 dark:border-white/10 dark:bg-white/3"
    >
      <div class="flex items-center justify-between gap-2 text-[10px] text-text-muted">
        <span> Showing {{ pageStart }}-{{ pageEnd }} of {{ sortedQuotas.length }} </span>
        <span> Page {{ page }} / {{ totalPages }} </span>
      </div>
      <div class="mt-1.5 flex items-center justify-end gap-1">
        <button
          type="button"
          :disabled="page === 1"
          class="flex h-6 items-center rounded-md border border-black/10 px-2 text-[10px] text-text-primary transition-colors hover:bg-black/5 disabled:cursor-not-allowed disabled:opacity-40 dark:border-white/10 dark:hover:bg-white/5"
          @click="page = Math.max(1, page - 1)"
        >
          Prev
        </button>
        <button
          type="button"
          :disabled="page === totalPages"
          class="flex h-6 items-center rounded-md border border-black/10 px-2 text-[10px] text-text-primary transition-colors hover:bg-black/5 disabled:cursor-not-allowed disabled:opacity-40 dark:border-white/10 dark:hover:bg-white/5"
          @click="page = Math.min(totalPages, page + 1)"
        >
          Next
        </button>
      </div>
    </div>
  </div>
</template>
