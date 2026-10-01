<script setup lang="ts">
import { cn } from "@/utils/cn";

const props = withDefaults(
	defineProps<{ size?: "sm" | "md" | "lg" | "xl"; className?: string }>(),
	{ size: "md" },
);

const sizes: Record<string, string> = {
	sm: "size-4",
	md: "size-6",
	lg: "size-8",
	xl: "size-12",
};
</script>

<template>
  <span :class="cn('material-symbols-outlined animate-spin text-brand-500', sizes[props.size], props.className)">
    progress_activity
  </span>
</template>
