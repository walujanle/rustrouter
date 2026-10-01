<script setup lang="ts">
import { cn } from "@/utils/cn";

const props = withDefaults(
	defineProps<{
		modelValue?: boolean;
		label?: string;
		description?: string;
		disabled?: boolean;
		size?: "sm" | "md" | "lg";
		className?: string;
		// An icon-only toggle has no text, so it needs an explicit name. These land
		// on the `role="switch"` button, not the root wrapper — a fallthrough
		// `title`/`aria-label` would land on the wrapper `<div>` and leave the
		// switch itself unnamed for assistive tech.
		title?: string;
		ariaLabel?: string;
	}>(),
	{ modelValue: false, disabled: false, size: "md" },
);

const emit = defineEmits<{ "update:modelValue": [value: boolean] }>();

const sizes: Record<string, { track: string; thumb: string; translate: string }> = {
	sm: { track: "w-8 h-4", thumb: "size-3", translate: "translate-x-4" },
	md: { track: "w-11 h-6", thumb: "size-5", translate: "translate-x-5" },
	lg: { track: "w-14 h-7", thumb: "size-6", translate: "translate-x-7" },
};

function handleClick() {
	if (!props.disabled) emit("update:modelValue", !props.modelValue);
}
</script>

<template>
  <div :class="cn('flex items-center gap-3', props.disabled && 'opacity-50 cursor-not-allowed', props.className)">
    <button
      type="button"
      role="switch"
      :aria-checked="props.modelValue"
      :aria-label="props.ariaLabel ?? props.label"
      :title="props.title"
      :disabled="props.disabled"
      :class="cn(
        'relative inline-flex shrink-0 cursor-pointer rounded-full',
        'transition-colors duration-200 ease-in-out',
        'focus:outline-none focus:ring-2 focus:ring-brand-500/30',
        props.modelValue ? 'bg-brand-500' : 'bg-surface-3',
        sizes[props.size].track,
        props.disabled && 'cursor-not-allowed',
      )"
      @click="handleClick"
    >
      <span
        :class="cn(
          'pointer-events-none inline-block rounded-full bg-white shadow-sm',
          'transform transition duration-200 ease-in-out',
          props.modelValue ? sizes[props.size].translate : 'translate-x-0.5',
          sizes[props.size].thumb,
          'mt-0.5',
        )"
      />
    </button>
    <div v-if="props.label || props.description" class="flex flex-col">
      <span v-if="props.label" class="text-sm font-medium text-text-main">{{ props.label }}</span>
      <span v-if="props.description" class="text-xs text-text-muted">{{ props.description }}</span>
    </div>
  </div>
</template>
