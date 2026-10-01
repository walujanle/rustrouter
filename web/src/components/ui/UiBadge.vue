<script setup lang="ts">
import { cn } from "@/utils/cn";

const props = withDefaults(
	defineProps<{
		variant?: "default" | "primary" | "success" | "warning" | "error" | "info";
		size?: "sm" | "md" | "lg";
		dot?: boolean;
		icon?: string;
		className?: string;
	}>(),
	{ variant: "default", size: "md", dot: false },
);

const variants: Record<string, string> = {
	default: "bg-surface-2 text-text-muted",
	primary: "bg-brand-500/10 text-brand-600 dark:text-brand-300",
	success: "bg-green-500/10 text-green-600 dark:text-green-400",
	warning: "bg-yellow-500/10 text-yellow-600 dark:text-yellow-400",
	error: "bg-red-500/10 text-red-600 dark:text-red-400",
	info: "bg-blue-500/10 text-blue-600 dark:text-blue-400",
};

const sizes: Record<string, string> = {
	sm: "px-2 py-0.5 text-[10px]",
	md: "px-2.5 py-1 text-xs",
	lg: "px-3 py-1.5 text-sm",
};

const dotColor: Record<string, string> = {
	success: "bg-green-500",
	warning: "bg-yellow-500",
	error: "bg-red-500",
	info: "bg-blue-500",
	primary: "bg-brand-500",
	default: "bg-gray-500",
};
</script>

<template>
  <span
    :class="cn(
      'inline-flex items-center gap-1.5 rounded-full font-semibold',
      variants[props.variant],
      sizes[props.size],
      props.className,
    )"
  >
    <span v-if="props.dot" :class="cn('size-1.5 rounded-full', dotColor[props.variant])" />
    <span v-if="props.icon" class="material-symbols-outlined text-[14px]">{{ props.icon }}</span>
    <slot />
  </span>
</template>
