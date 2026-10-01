<script setup lang="ts">
import { cn } from "@/utils/cn";

const props = withDefaults(
	defineProps<{
		title?: string;
		subtitle?: string;
		icon?: string;
		padding?: "none" | "xs" | "sm" | "md" | "lg";
		hover?: boolean;
		elev?: boolean;
		className?: string;
	}>(),
	{ padding: "md", hover: false, elev: false },
);

const paddings: Record<string, string> = {
	none: "",
	xs: "p-3",
	sm: "p-4",
	md: "p-6",
	lg: "p-8",
};
</script>

<template>
  <div
    :class="cn(
      'bg-surface border border-border-subtle',
      props.elev ? 'rounded-[14px] shadow-elev' : 'rounded-[14px] shadow-soft',
      props.hover && 'hover:shadow-warm hover:border-brand-500/30 transition-all cursor-pointer',
      paddings[props.padding],
      props.className,
    )"
  >
    <div v-if="props.title || $slots.action" class="flex items-center justify-between mb-4">
      <div class="flex items-center gap-3">
        <div v-if="props.icon" class="p-2 rounded-[10px] bg-bg text-text-muted">
          <span class="material-symbols-outlined text-[20px]">{{ props.icon }}</span>
        </div>
        <div>
          <h3 v-if="props.title" class="text-text-main font-semibold">{{ props.title }}</h3>
          <p v-if="props.subtitle" class="text-sm text-text-muted">{{ props.subtitle }}</p>
        </div>
      </div>
      <slot name="action" />
    </div>
    <slot />
  </div>
</template>
