<script setup lang="ts">
import { useId } from "vue";
import { cn } from "@/utils/cn";

defineOptions({ inheritAttrs: false });

const uid = useId();

const props = withDefaults(
	defineProps<{
		label?: string;
		options?: Array<{ value: string; label: string }>;
		modelValue?: string;
		placeholder?: string;
		error?: string;
		hint?: string;
		disabled?: boolean;
		required?: boolean;
		className?: string;
		selectClassName?: string;
	}>(),
	{ options: () => [], placeholder: "Select an option", disabled: false, required: false },
);

defineEmits<{ "update:modelValue": [value: string] }>();
</script>

<template>
  <div :class="cn('flex flex-col gap-1.5', props.className)">
    <label v-if="props.label" :for="`select-${uid}`" class="text-sm font-medium text-text-main">
      {{ props.label }}
      <span v-if="props.required" class="text-red-500 ml-1">*</span>
    </label>
    <div class="relative">
      <select
        v-bind="$attrs"
        :id="`select-${uid}`"
        :value="props.modelValue"
        :disabled="props.disabled"
        :class="cn(
          'w-full py-2.5 px-3 pr-10 text-sm text-text-main',
          'bg-surface-2 border border-transparent rounded-[10px] appearance-none',
          'focus:outline-none focus:ring-2 focus:ring-brand-500/30 focus:border-brand-500/40',
          'transition-all duration-150 disabled:opacity-50 disabled:cursor-not-allowed',
          'text-[16px] sm:text-sm',
          props.error && 'ring-1 ring-red-500 focus:ring-2 focus:ring-red-500/40 border-red-500/40',
          props.selectClassName,
        )"
        @change="$emit('update:modelValue', ($event.target as HTMLSelectElement).value)"
      >
        <option value="" disabled>{{ props.placeholder }}</option>
        <option v-for="option in props.options" :key="option.value" :value="option.value">
          {{ option.label }}
        </option>
      </select>
      <div class="absolute inset-y-0 right-0 flex items-center pr-3 pointer-events-none text-text-muted">
        <span class="material-symbols-outlined text-[20px]">expand_more</span>
      </div>
    </div>
    <p v-if="props.error" class="text-xs text-red-500 flex items-center gap-1">
      <span class="material-symbols-outlined text-[14px]">error</span>
      {{ props.error }}
    </p>
    <p v-else-if="props.hint" class="text-xs text-text-muted">{{ props.hint }}</p>
  </div>
</template>
