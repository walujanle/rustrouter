<script setup lang="ts">
import { computed } from "vue";

import { CAPACITY_META } from "@/constants/models";
import Tooltip from "./UiTooltip.vue";

const props = withDefaults(
	defineProps<{
		caps?: Record<string, any> | null;
		className?: string;
		colorOverride?: string;
		size?: number;
	}>(),
	{ className: "", size: 16 },
);

const active = computed(() =>
	props.caps ? Object.keys(CAPACITY_META).filter((k) => props.caps?.[k]) : [],
);
</script>

<template>
  <span v-if="active.length > 0" class="inline-flex items-center gap-0.5" :class="props.className">
    <Tooltip
      v-for="k in active"
      :key="k"
      :text="`${CAPACITY_META[k as keyof typeof CAPACITY_META].label} — ${CAPACITY_META[k as keyof typeof CAPACITY_META].desc}`"
    >
      <span
        class="material-symbols-outlined leading-none cursor-help"
        :class="props.colorOverride || CAPACITY_META[k as keyof typeof CAPACITY_META].color"
        :style="{ fontSize: `${props.size}px` }"
      >
        {{ CAPACITY_META[k as keyof typeof CAPACITY_META].icon }}
      </span>
    </Tooltip>
  </span>
</template>
