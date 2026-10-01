<script setup lang="ts">
import { computed } from "vue";

import Card from "@/components/ui/UiCard.vue";

// Renders the per-kind config blob as a two-column key/value card.

type Formatter = (v: any) => string;

const FIELD_SCHEMA: Record<string, { label: string; format: Formatter; isLink?: boolean; mono?: boolean }> = {
	mode: { label: "Mode", format: (v) => v },
	defaultModel: { label: "Model", format: (v) => v, mono: true },
	baseUrl: { label: "Endpoint", format: (v) => v, isLink: true, mono: true },
	costPerQuery: { label: "Cost / call", format: (v) => (v === 0 ? "Free" : `$${Number(v).toFixed(4)}`) },
	pricingUrl: { label: "Pricing", format: () => "View pricing", isLink: true },
	freeTier: { label: "Free tier", format: (v) => v },
	freeMonthlyQuota: {
		label: "Free quota",
		format: (v) => (v === 0 ? "—" : v >= 999999 ? "Unlimited" : `${Number(v).toLocaleString("en-US")} / mo`),
	},
	searchTypes: { label: "Types", format: (v) => v.join(", ") },
	formats: { label: "Formats", format: (v) => v.join(", ") },
	maxMaxResults: { label: "Max results", format: (v) => v },
	maxCharacters: { label: "Max chars", format: (v) => Number(v).toLocaleString("en-US") },
};

const props = withDefaults(
	defineProps<{
		config?: Record<string, any> | null;
		provider?: Record<string, any> | null;
		title?: string;
	}>(),
	{ title: "Provider Info", config: null, provider: null },
);

interface Row {
	key: string;
	label: string;
	value: string;
	isLink?: boolean;
	mono?: boolean;
	raw: any;
}

const rows = computed<Row[]>(() => {
	const config = props.config;
	if (!config) return [];
	return Object.entries(FIELD_SCHEMA)
		.filter(([key]) => config[key] !== undefined && config[key] !== null && config[key] !== "")
		.map(([key, schema]) => ({
			key,
			label: schema.label,
			value: schema.format(config[key]),
			isLink: schema.isLink,
			mono: schema.mono,
			raw: config[key],
		}));
});

const signupUrl = computed(() => props.provider?.notice?.apiKeyUrl || props.provider?.website);
const noticeText = computed(() => props.provider?.notice?.text);
</script>

<template>
  <Card v-if="props.config">
    <div class="flex items-center justify-between mb-3">
      <h2 class="text-lg font-semibold">{{ props.title }}</h2>
      <a
        v-if="signupUrl"
        :href="signupUrl"
        target="_blank"
        rel="noopener noreferrer"
        class="text-xs text-primary hover:underline inline-flex items-center gap-1"
      >
        <span class="material-symbols-outlined text-sm">open_in_new</span>
        Get API Key
      </a>
    </div>
    <div class="grid grid-cols-1 sm:grid-cols-2 gap-x-6 gap-y-2">
      <div v-for="r in rows" :key="r.key" class="flex items-center gap-3 min-w-0">
        <span class="text-xs text-text-muted w-28 shrink-0">{{ r.label }}</span>
        <a
          v-if="r.isLink"
          :href="r.raw"
          target="_blank"
          rel="noopener noreferrer"
          :class="['text-sm text-primary hover:underline truncate', r.mono && 'font-mono']"
        >
          {{ r.value }}
        </a>
        <span v-else :class="['text-sm text-text-main truncate', r.mono && 'font-mono']">
          {{ r.value }}
        </span>
      </div>
      <div v-if="noticeText" class="flex items-start gap-3 min-w-0 sm:col-span-2">
        <span class="text-xs text-text-muted w-28 shrink-0 mt-0.5">Notice</span>
        <span class="text-sm text-text-main leading-relaxed">{{ noticeText }}</span>
      </div>
    </div>
  </Card>
</template>
