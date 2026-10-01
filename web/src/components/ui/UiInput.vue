<script setup lang="ts">
import { useId } from "vue";
import { cn } from "@/utils/cn";

defineOptions({ inheritAttrs: false });

const uid = useId();

const props = withDefaults(
	defineProps<{
		label?: string;
		type?: string;
		placeholder?: string;
		modelValue?: string | number;
		error?: string;
		hint?: string;
		icon?: string;
		disabled?: boolean;
		required?: boolean;
		className?: string;
		inputClassName?: string;
	}>(),
	{ type: "text", disabled: false, required: false },
);

defineEmits<{ "update:modelValue": [value: string] }>();
</script>

<template>
  <div :class="cn('flex flex-col gap-1.5', props.className)">
    <label v-if="props.label" :for="`input-${uid}`" class="text-sm font-medium text-text-main">
      {{ props.label }}
      <span v-if="props.required" class="text-red-500 ml-1">*</span>
    </label>
    <div class="relative">
      <div v-if="props.icon" class="absolute inset-y-0 left-0 flex items-center pl-3 pointer-events-none text-text-muted">
        <span class="material-symbols-outlined text-[20px]">{{ props.icon }}</span>
      </div>
      <input
        v-bind="$attrs"
        :id="`input-${uid}`"
        :type="props.type"
        :placeholder="props.placeholder"
        :value="props.modelValue"
        :disabled="props.disabled"
        :class="cn(
          'w-full py-2.5 px-3 text-sm text-text-main bg-surface-2 rounded-[10px]',
          'border border-transparent placeholder-text-muted/70',
          'focus:outline-none focus:ring-2 focus:ring-brand-500/30 focus:border-brand-500/40',
          'transition-all duration-150 ease-out disabled:opacity-50 disabled:cursor-not-allowed',
          'text-[16px] sm:text-sm',
          props.icon && 'pl-10',
          props.error && 'ring-1 ring-red-500 focus:ring-2 focus:ring-red-500/40 border-red-500/40',
          props.inputClassName,
        )"
        @input="$emit('update:modelValue', ($event.target as HTMLInputElement).value)"
      />
    </div>
    <p v-if="props.error" class="text-xs text-red-500 flex items-center gap-1">
      <span class="material-symbols-outlined text-[14px]">error</span>
      {{ props.error }}
    </p>
    <p v-else-if="props.hint || $slots.hint" class="text-xs text-text-muted">
      <slot name="hint">{{ props.hint }}</slot>
    </p>
  </div>
</template>
