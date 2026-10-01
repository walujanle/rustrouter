<script setup lang="ts">
import { cn } from "@/utils/cn";

const props = withDefaults(
	defineProps<{
		options?: Array<{ value: string; label: string; icon?: string }>;
		modelValue?: string;
		size?: "sm" | "md" | "lg";
		className?: string;
	}>(),
	{ options: () => [], size: "md" },
);

defineEmits<{ "update:modelValue": [value: string] }>();

const sizes: Record<string, string> = {
	sm: "h-7 text-xs",
	md: "h-9 text-sm",
	lg: "h-11 text-base",
};
</script>

<template>
  <div :class="cn('inline-flex items-center p-1 rounded-[10px] overflow-x-auto', 'bg-surface-2', props.className)">
    <button
      v-for="option in props.options"
      :key="option.value"
      type="button"
      :class="cn(
        'shrink-0 px-4 rounded-lg font-medium transition-all',
        sizes[props.size],
        props.modelValue === option.value ? 'bg-surface text-text-main shadow-sm' : 'text-text-muted hover:text-text-main',
      )"
      @click="$emit('update:modelValue', option.value)"
    >
      <span v-if="option.icon" class="material-symbols-outlined text-[16px] mr-1.5">{{ option.icon }}</span>
      {{ option.label }}
    </button>
  </div>
</template>
