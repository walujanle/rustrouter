<script setup lang="ts">
import Card from "@/components/ui/UiCard.vue";

const props = defineProps<{ stats: Record<string, any> }>();

const fmt = (n?: number) => new Intl.NumberFormat("en-US").format(n || 0);
const fmtCost = (n?: number) => `$${(n || 0).toFixed(2)}`;
</script>

<template>
  <div class="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-5 sm:gap-4">
    <Card class="flex min-w-0 flex-col items-center text-center gap-1 px-3 py-3 sm:px-4">
      <span class="text-text-muted text-xs uppercase font-semibold sm:text-sm">Total Requests</span>
      <span class="w-full truncate text-lg font-bold xl:text-xl" :title="fmt(props.stats.totalRequests)">{{ fmt(props.stats.totalRequests) }}</span>
    </Card>
    <Card class="flex min-w-0 flex-col items-center text-center gap-1 px-3 py-3 sm:px-4">
      <span class="text-text-muted text-xs uppercase font-semibold sm:text-sm">Total Input Tokens</span>
      <span class="w-full truncate text-lg font-bold text-primary xl:text-xl" :title="fmt(props.stats.totalPromptTokens)">{{ fmt(props.stats.totalPromptTokens) }}</span>
    </Card>
    <Card class="flex min-w-0 flex-col items-center text-center gap-1 px-3 py-3 sm:px-4">
      <span class="text-text-muted text-xs uppercase font-semibold sm:text-sm">Cached Tokens</span>
      <span class="w-full truncate text-lg font-bold text-info xl:text-xl" :title="fmt(props.stats.totalCachedTokens)">{{ fmt(props.stats.totalCachedTokens) }}</span>
    </Card>
    <Card class="flex min-w-0 flex-col items-center text-center gap-1 px-3 py-3 sm:px-4">
      <span class="text-text-muted text-xs uppercase font-semibold sm:text-sm">Output Tokens</span>
      <span class="w-full truncate text-lg font-bold text-success xl:text-xl" :title="fmt(props.stats.totalCompletionTokens)">{{ fmt(props.stats.totalCompletionTokens) }}</span>
    </Card>
    <Card class="flex min-w-0 flex-col items-center text-center gap-1 px-3 py-3 sm:px-4">
      <span class="text-text-muted text-xs uppercase font-semibold sm:text-sm">Est. Cost</span>
      <span class="w-full truncate text-lg font-bold text-warning xl:text-xl" :title="`~${fmtCost(props.stats.totalCost)}`">~{{ fmtCost(props.stats.totalCost) }}</span>
      <span class="text-[10px] text-text-muted">Estimated, not actual billing</span>
    </Card>
  </div>
</template>
