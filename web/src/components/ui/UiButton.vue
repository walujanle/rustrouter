<script setup lang="ts">
import { cn } from "@/utils/cn";

defineOptions({ inheritAttrs: false });

const props = withDefaults(
	defineProps<{
		variant?: "primary" | "secondary" | "outline" | "ghost" | "danger" | "success";
		size?: "sm" | "md" | "lg";
		type?: "button" | "submit" | "reset";
		icon?: string;
		iconRight?: string;
		disabled?: boolean;
		loading?: boolean;
		fullWidth?: boolean;
		className?: string;
	}>(),
	{ variant: "primary", size: "md", type: "button", disabled: false, loading: false, fullWidth: false },
);

const variants: Record<string, string> = {
	primary:
		"bg-brand-500 hover:bg-brand-600 text-white shadow-sm disabled:bg-surface-3 disabled:text-text-muted",
	secondary:
		"bg-surface-2 hover:bg-surface-3 text-text-main border border-border disabled:opacity-50",
	outline: "border border-border text-text-main hover:bg-surface-2 hover:border-brand-500/40",
	ghost: "text-text-muted hover:bg-surface-2 hover:text-text-main",
	danger:
		"bg-red-500 hover:bg-red-600 text-white shadow-sm disabled:bg-surface-3 disabled:text-text-muted",
	success:
		"bg-green-600 hover:bg-green-700 text-white shadow-sm disabled:bg-surface-3 disabled:text-text-muted",
};

const sizes: Record<string, string> = {
	sm: "h-7 px-3 text-xs rounded-lg",
	md: "h-9 px-4 text-sm rounded-[10px]",
	lg: "h-11 px-6 text-sm rounded-[10px]",
};
</script>

<template>
  <button
    v-bind="$attrs"
    :type="props.type"
    :class="cn(
      'inline-flex items-center justify-center gap-2 font-semibold transition-all duration-150 ease-out cursor-pointer',
      'active:scale-[0.97] disabled:opacity-50 disabled:cursor-not-allowed disabled:active:scale-100',
      variants[props.variant],
      sizes[props.size],
      props.fullWidth && 'w-full',
      props.className,
    )"
    :disabled="props.disabled || props.loading"
  >
    <span v-if="props.loading" class="material-symbols-outlined animate-spin text-[18px]">progress_activity</span>
    <span v-else-if="props.icon" class="material-symbols-outlined text-[18px]">{{ props.icon }}</span>
    <slot />
    <span v-if="props.iconRight && !props.loading" class="material-symbols-outlined text-[18px]">{{ props.iconRight }}</span>
  </button>
</template>
