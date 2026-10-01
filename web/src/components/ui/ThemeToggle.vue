<script setup lang="ts">
import { useTheme } from "@/hooks/useTheme";
import { cn } from "@/utils/cn";

const props = withDefaults(
	defineProps<{ className?: string; variant?: "default" | "card" }>(),
	{ variant: "default" },
);

const { isDark, toggleTheme } = useTheme();

const variants: Record<string, string> = {
	default: cn(
		"flex items-center justify-center size-10 rounded-full",
		"text-text-muted hover:text-text-main",
		"hover:bg-surface-2 transition-colors",
	),
	card: cn(
		"flex items-center justify-center size-11 rounded-full",
		"bg-surface/60 hover:bg-surface",
		"border border-border",
		"backdrop-blur-md shadow-sm hover:shadow-warm",
		"text-text-muted hover:text-brand-500",
		"transition-all group",
	),
};
</script>

<template>
  <button
    type="button"
    :class="cn(variants[props.variant], props.className)"
    :aria-label="`Switch to ${isDark ? 'light' : 'dark'} mode`"
    :title="`Switch to ${isDark ? 'light' : 'dark'} mode`"
    @click="toggleTheme()"
  >
    <span
      :class="cn(
        'material-symbols-outlined text-[22px]',
        props.variant === 'card' && 'transition-transform duration-300 group-hover:rotate-12',
      )"
    >
      {{ isDark ? "light_mode" : "dark_mode" }}
    </span>
  </button>
</template>
