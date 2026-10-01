<script setup lang="ts">
import {
	CategoryScale,
	Chart as ChartJS,
	Tooltip as ChartTooltip,
	Filler,
	Legend,
	LinearScale,
	LineElement,
	PointElement,
} from "chart.js";
import { computed, onMounted, ref, watch } from "vue";
import { Line } from "vue-chartjs";

import Card from "@/components/ui/UiCard.vue";

ChartJS.register(CategoryScale, LinearScale, LineElement, PointElement, ChartTooltip, Legend, Filler);

const props = withDefaults(defineProps<{ period?: string }>(), { period: "7d" });

const data = ref<Array<Record<string, any>>>([]);
const loading = ref(true);
const viewMode = ref("tokens");

const fmtTokens = (n: number) => {
	if (n >= 1000000) return `${(n / 1000000).toFixed(1)}M`;
	if (n >= 1000) return `${(n / 1000).toFixed(1)}K`;
	return String(n || 0);
};
const fmtCost = (n: number) => `$${(n || 0).toFixed(4)}`;
const fmtRequests = (n: number) => String(n || 0);

const VIEW_MODES = [
	{ value: "tokens", label: "Tokens" },
	{ value: "requests", label: "Requests" },
	{ value: "cost", label: "Cost" },
];

const VIEW_CONFIG: Record<string, { dataKey: string; color: string; formatter: (n: number) => string; label: string }> = {
	tokens: { dataKey: "tokens", color: "#6366f1", formatter: fmtTokens, label: "Tokens" },
	requests: { dataKey: "requests", color: "#14b8a6", formatter: fmtRequests, label: "Requests" },
	cost: { dataKey: "cost", color: "#f59e0b", formatter: fmtCost, label: "Cost" },
};

async function fetchData() {
	loading.value = true;
	try {
		const res = await fetch(`/api/usage/chart?period=${props.period}`);
		if (res.ok) data.value = await res.json();
	} catch (e) {
		console.error("Failed to fetch chart data:", e);
	} finally {
		loading.value = false;
	}
}

onMounted(fetchData);
watch(() => props.period, fetchData);

const cfg = computed(() => VIEW_CONFIG[viewMode.value]);
const hasData = computed(() => data.value.some((d) => (d[cfg.value.dataKey] || 0) > 0));

function hexToRgba(hex: string, alpha: number) {
	const n = Number.parseInt(hex.slice(1), 16);
	return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${alpha})`;
}

function tickColor() {
	const v = getComputedStyle(document.documentElement).getPropertyValue("--color-text-muted").trim();
	return v || "#6B7280";
}

const chartData = computed(() => ({
	labels: data.value.map((d) => d.label),
	datasets: [
		{
			data: data.value.map((d) => d[cfg.value.dataKey] || 0),
			borderColor: cfg.value.color,
			borderWidth: 2,
			pointRadius: 0,
			pointHoverRadius: 4,
			tension: 0.4,
			fill: true,
			backgroundColor: (context: any) => {
				const { ctx, chartArea } = context.chart;
				if (!chartArea) return "transparent";
				const g = ctx.createLinearGradient(0, chartArea.top, 0, chartArea.bottom);
				g.addColorStop(0.05, hexToRgba(cfg.value.color, 0.25));
				g.addColorStop(0.95, hexToRgba(cfg.value.color, 0));
				return g;
			},
		},
	],
}));

const chartOptions = computed(() => {
	const c = cfg.value;
	return {
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
				titleFont: { size: 12 },
				displayColors: false,
				callbacks: {
					label: (ctx: any) => `${c.label}: ${c.formatter(ctx.parsed.y)}`,
				},
			},
		},
		scales: {
			x: {
				grid: { color: "rgba(128,128,128,0.1)" },
				border: { display: false },
				ticks: { color: tickColor(), font: { size: 10 }, maxRotation: 0 },
			},
			y: {
				grid: { color: "rgba(128,128,128,0.1)" },
				border: { display: false },
				ticks: { color: tickColor(), font: { size: 10 }, callback: (v: any) => c.formatter(Number(v)) },
			},
		},
	};
});
</script>

<template>
  <Card class="flex min-w-0 flex-col gap-3 p-3 sm:p-4">
    <div
      class="grid w-full items-center gap-1 rounded-lg border border-border bg-surface-2 p-1 sm:w-auto sm:self-start"
      :style="{ gridTemplateColumns: `repeat(${VIEW_MODES.length}, minmax(0, 1fr))` }"
    >
      <button
        v-for="m in VIEW_MODES"
        :key="m.value"
        type="button"
        :class="`px-3 py-1 rounded-md text-sm font-medium transition-colors ${viewMode === m.value ? 'bg-primary text-white shadow-sm' : 'text-text-muted hover:text-text hover:bg-surface-3'}`"
        @click="viewMode = m.value"
      >
        {{ m.label }}
      </button>
    </div>

    <div v-if="loading" class="h-48 flex items-center justify-center text-text-muted text-sm">Loading...</div>
    <div v-else-if="!hasData" class="h-48 flex items-center justify-center text-text-muted text-sm">No data for this period</div>
    <div v-else class="h-55 w-full">
      <Line :data="chartData" :options="chartOptions" />
    </div>
  </Card>
</template>
