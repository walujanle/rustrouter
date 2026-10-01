<script setup lang="ts">
import { BarElement, CategoryScale, Chart as ChartJS, Tooltip as ChartTooltip, Legend, LinearScale } from "chart.js";
import { computed, ref } from "vue";
import { Bar } from "vue-chartjs";

import Card from "@/components/ui/UiCard.vue";

ChartJS.register(CategoryScale, LinearScale, BarElement, ChartTooltip, Legend);

const props = defineProps<{ byProvider?: Record<string, any> }>();

const COLORS = ["#6366f1", "#14b8a6", "#f59e0b", "#ef4444", "#8b5cf6", "#06b6d4", "#10b981", "#f97316"];

const fmtTokens = (n: number) => {
	if (n >= 1000000) return `${(n / 1000000).toFixed(1)}M`;
	if (n >= 1000) return `${(n / 1000).toFixed(1)}K`;
	return String(n || 0);
};

const viewMode = ref<"tokens" | "requests">("tokens");

const chartData = computed(() => {
	if (!props.byProvider) return [];
	return Object.entries(props.byProvider)
		.map(([id, data]: [string, any]) => ({
			name: id,
			tokens: (data.promptTokens || 0) + (data.completionTokens || 0),
			requests: data.requests || 0,
		}))
		.filter((d) => (d as any)[viewMode.value] > 0)
		.sort((a, b) => (b as any)[viewMode.value] - (a as any)[viewMode.value]);
});

const fmt = computed(() => (viewMode.value === "tokens" ? fmtTokens : String));
const label = computed(() => (viewMode.value === "tokens" ? "Tokens" : "Requests"));

function tickColor() {
	return getComputedStyle(document.documentElement).getPropertyValue("--color-text-muted").trim() || "#6B7280";
}

function hexToRgba(hex: string, alpha: number) {
	const n = Number.parseInt(hex.slice(1), 16);
	return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${alpha})`;
}

const barData = computed(() => ({
	labels: chartData.value.map((d) => d.name),
	datasets: [
		{
			data: chartData.value.map((d) => (d as any)[viewMode.value]),
			backgroundColor: chartData.value.map((_, i) => hexToRgba(COLORS[i % COLORS.length], 0.85)),
			hoverBackgroundColor: chartData.value.map((_, i) => COLORS[i % COLORS.length]),
			borderRadius: { topLeft: 4, topRight: 4, bottomRight: 0, bottomLeft: 0 },
			borderSkipped: false,
		},
	],
}));

const barOptions = computed(() => ({
	responsive: true,
	maintainAspectRatio: false,
	plugins: {
		legend: { display: false },
		tooltip: {
			backgroundColor: getComputedStyle(document.documentElement).getPropertyValue("--color-bg").trim() || "#ffffff",
			borderColor: getComputedStyle(document.documentElement).getPropertyValue("--color-border").trim() || "#e5e7eb",
			borderWidth: 1,
			cornerRadius: 8,
			bodyFont: { size: 12 },
			displayColors: false,
			callbacks: { label: (ctx: any) => `${label.value}: ${fmt.value(ctx.parsed.y)}` },
		},
	},
	scales: {
		x: {
			grid: { display: false },
			border: { display: false },
			ticks: {
				color: tickColor(),
				font: { size: 10 },
				autoSkip: false,
				maxRotation: 0,
				callback(this: any, value: any) {
					const v = String(this.getLabelForValue(value));
					return v.length > 10 ? `${v.slice(0, 10)}…` : v;
				},
			},
		},
		y: {
			grid: { color: "rgba(128,128,128,0.1)" },
			border: { display: false },
			ticks: { color: tickColor(), font: { size: 10 }, callback: (v: any) => fmt.value(Number(v)) },
		},
	},
}));
</script>

<template>
  <Card class="flex min-w-0 flex-col gap-3 p-3 sm:p-4">
    <div class="flex items-center justify-between gap-2">
      <span class="text-sm font-semibold text-text-muted uppercase tracking-wide">By Provider</span>
      <div class="grid grid-cols-2 items-center gap-1 rounded-lg border border-border bg-surface-2 p-1">
        <button
          type="button"
          :class="`px-2.5 py-0.5 rounded-md text-xs font-medium transition-colors ${viewMode === 'tokens' ? 'bg-primary text-white shadow-sm' : 'text-text-muted hover:text-text hover:bg-surface-3'}`"
          @click="viewMode = 'tokens'"
        >
          Tokens
        </button>
        <button
          type="button"
          :class="`px-2.5 py-0.5 rounded-md text-xs font-medium transition-colors ${viewMode === 'requests' ? 'bg-primary text-white shadow-sm' : 'text-text-muted hover:text-text hover:bg-surface-3'}`"
          @click="viewMode = 'requests'"
        >
          Requests
        </button>
      </div>
    </div>

    <div v-if="!chartData.length" class="h-44 flex items-center justify-center text-text-muted text-sm">No provider usage yet</div>
    <div v-else class="h-45 w-full">
      <Bar :data="barData" :options="barOptions" />
    </div>
  </Card>
</template>
