<script lang="ts">
export const fmt = (n?: number) => new Intl.NumberFormat().format(n || 0);
export const fmtCost = (n?: number) => `$${(n || 0).toFixed(2)}`;

export function fmtTime(iso?: string | null): string {
	if (!iso) return "Never";
	const diffMins = Math.floor((Date.now() - new Date(iso).getTime()) / 60000);
	if (diffMins < 1) return "Just now";
	if (diffMins < 60) return `${diffMins}m ago`;
	if (diffMins < 1440) return `${Math.floor(diffMins / 60)}h ago`;
	return new Date(iso).toLocaleDateString();
}
</script>

<script setup lang="ts">
import { defineComponent, h, onMounted, ref, watch } from "vue";

import Card from "@/components/ui/UiCard.vue";

interface Column {
	field: string;
	label: string;
	align?: string;
}

const props = withDefaults(
	defineProps<{
		title: string;
		columns: Column[];
		groupedData: Array<Record<string, any>>;
		tableType: string;
		sortBy: string;
		sortOrder: string;
		viewMode: string;
		storageKey: string;
		emptyMessage: string;
	}>(),
	{},
);

const emit = defineEmits<{ "toggle-sort": [tableType: string, field: string] }>();

// Render 3 token or cost cells based on viewMode.
const ValueCells = defineComponent({
	props: {
		item: { type: Object as () => Record<string, any>, required: true },
		viewMode: { type: String, required: true },
		isSummary: { type: Boolean, default: false },
	},
	setup(p) {
		return () => {
			const item = p.item;
			if (p.viewMode === "tokens") {
				return [
					h("td", { class: "px-6 py-3 text-right text-text-muted" }, p.isSummary && item.promptTokens === undefined ? "—" : fmt(item.promptTokens)),
					h("td", { class: "px-6 py-3 text-right text-text-muted" }, item.cachedTokens ? fmt(item.cachedTokens) : "—"),
					h("td", { class: "px-6 py-3 text-right text-text-muted" }, p.isSummary && item.completionTokens === undefined ? "—" : fmt(item.completionTokens)),
					h("td", { class: "px-6 py-3 text-right font-medium" }, fmt(item.totalTokens)),
				];
			}
			return [
				h("td", { class: "px-6 py-3 text-right text-text-muted" }, p.isSummary && item.inputCost === undefined ? "—" : fmtCost(item.inputCost)),
				h("td", { class: "px-6 py-3 text-right text-text-muted" }, item.cachedCost ? fmtCost(item.cachedCost) : "—"),
				h("td", { class: "px-6 py-3 text-right text-text-muted" }, p.isSummary && item.outputCost === undefined ? "—" : fmtCost(item.outputCost)),
				h("td", { class: "px-6 py-3 text-right font-medium text-warning" }, fmtCost(item.totalCost || item.cost)),
			];
		};
	},
});

const expanded = ref<Set<string>>(new Set());

onMounted(() => {
	try {
		const saved = localStorage.getItem(props.storageKey);
		if (saved) expanded.value = new Set(JSON.parse(saved));
	} catch (e) {
		console.error(`Failed to load ${props.storageKey}:`, e);
	}
});

watch(
	() => expanded.value,
	(value) => {
		try {
			localStorage.setItem(props.storageKey, JSON.stringify([...value]));
		} catch (e) {
			console.error(`Failed to save ${props.storageKey}:`, e);
		}
	},
);

function toggleGroup(groupKey: string) {
	const next = new Set(expanded.value);
	next.has(groupKey) ? next.delete(groupKey) : next.add(groupKey);
	expanded.value = next;
}

function ariaSort(field: string): "ascending" | "descending" | "none" {
	if (props.sortBy !== field) return "none";
	return props.sortOrder === "asc" ? "ascending" : "descending";
}

const valueColumns: Record<string, Column[]> = {
	tokens: [
		{ field: "promptTokens", label: "Input Tokens" },
		{ field: "cachedTokens", label: "Cached" },
		{ field: "completionTokens", label: "Output Tokens" },
		{ field: "totalTokens", label: "Total Tokens" },
	],
	costs: [
		{ field: "promptTokens", label: "Input Cost" },
		{ field: "cachedCost", label: "Cached Cost" },
		{ field: "completionTokens", label: "Output Cost" },
		{ field: "cost", label: "Total Cost" },
	],
};
</script>

<template>
  <Card class="overflow-hidden">
    <div class="p-4 border-b border-border bg-surface-2/50">
      <h3 class="font-semibold">{{ props.title }}</h3>
    </div>
    <div class="overflow-x-auto">
      <table class="w-full text-sm text-left">
        <thead class="bg-surface-2/30 text-text-muted uppercase text-xs">
          <tr>
            <th
              v-for="col in props.columns"
              :key="col.field"
              class="p-0"
              :aria-sort="ariaSort(col.field)"
            >
              <button
                type="button"
                :class="`w-full px-6 py-3 text-xs font-medium uppercase tracking-wide cursor-pointer hover:bg-surface-2/50 ${col.align === 'right' ? 'text-right' : 'text-left'}`"
                @click="emit('toggle-sort', props.tableType, col.field)"
              >
                {{ col.label }}
                <span v-if="props.sortBy !== col.field" class="ml-1 opacity-20">↕</span>
                <span v-else class="ml-1">{{ props.sortOrder === "asc" ? "↑" : "↓" }}</span>
              </button>
            </th>
            <th
              v-for="col in valueColumns[props.viewMode]"
              :key="col.field"
              class="p-0"
              :aria-sort="ariaSort(col.field)"
            >
              <button
                type="button"
                class="w-full px-6 py-3 text-xs font-medium uppercase tracking-wide text-right cursor-pointer hover:bg-surface-2/50"
                @click="emit('toggle-sort', props.tableType, col.field)"
              >
                {{ col.label }}
                <span v-if="props.sortBy !== col.field" class="ml-1 opacity-20">↕</span>
                <span v-else class="ml-1">{{ props.sortOrder === "asc" ? "↑" : "↓" }}</span>
              </button>
            </th>
          </tr>
        </thead>
        <tbody class="divide-y divide-border">
          <template v-for="group in props.groupedData" :key="group.groupKey">
            <tr
              class="group-summary cursor-pointer hover:bg-surface-2/50 transition-colors"
              tabindex="0"
              :aria-expanded="expanded.has(group.groupKey)"
              @click="toggleGroup(group.groupKey)"
              @keydown.enter.prevent="toggleGroup(group.groupKey)"
              @keydown.space.prevent="toggleGroup(group.groupKey)"
            >
              <td class="px-6 py-3">
                <div class="flex items-center gap-2">
                  <span
                    :class="`material-symbols-outlined text-[18px] text-text-muted transition-transform ${expanded.has(group.groupKey) ? 'rotate-90' : ''}`"
                  >
                    chevron_right
                  </span>
                  <span :class="`font-medium transition-colors ${group.summary.pending > 0 ? 'text-primary' : ''}`">
                    {{ group.groupKey }}
                  </span>
                </div>
              </td>
              <slot name="summary-cells" :group="group" />
              <ValueCells :item="group.summary" :view-mode="props.viewMode" is-summary />
            </tr>
            <template v-if="expanded.has(group.groupKey)">
              <tr
                v-for="item in group.items"
                :key="`detail-${item.key}`"
                class="group-detail hover:bg-surface-2/20 transition-colors"
              >
                <slot name="detail-cells" :item="item" />
                <ValueCells :item="item" :view-mode="props.viewMode" />
              </tr>
            </template>
          </template>
          <tr v-if="props.groupedData.length === 0">
            <td :colspan="props.columns.length + valueColumns[props.viewMode].length" class="px-6 py-8 text-center text-text-muted">
              {{ props.emptyMessage }}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </Card>
</template>
