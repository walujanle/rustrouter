<script setup lang="ts">
import { nextTick, onBeforeUnmount, ref, watch } from "vue";

import { cn } from "@/utils/cn";
import { popOverlay, pushOverlay } from "@/utils/overlayStack";
import Tooltip from "./UiTooltip.vue";

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		title?: string;
		size?: "sm" | "md" | "lg" | "xl" | "full";
		closeOnOverlay?: boolean;
		showTrafficLights?: boolean;
		className?: string;
	}>(),
	{ size: "md", closeOnOverlay: true, showTrafficLights: true },
);

const emit = defineEmits<{ close: [] }>();

const sizes: Record<string, string> = {
	sm: "max-w-sm",
	md: "max-w-md",
	lg: "max-w-lg",
	xl: "max-w-xl",
	full: "max-w-4xl",
};

const dialogRef = ref<HTMLElement | null>(null);
let token: symbol | null = null;
let previouslyFocused: HTMLElement | null = null;

watch(
	() => props.isOpen,
	(open) => {
		if (open && !token) {
			previouslyFocused = document.activeElement as HTMLElement | null;
			token = pushOverlay({
				onEscape: () => emit("close"),
				container: () => dialogRef.value,
			});
			nextTick(() => dialogRef.value?.focus());
		} else if (!open && token) {
			popOverlay(token);
			token = null;
			previouslyFocused?.focus?.();
			previouslyFocused = null;
		}
	},
	{ immediate: true },
);

onBeforeUnmount(() => {
	if (token) popOverlay(token);
});
</script>

<template>
  <Teleport to="body">
    <div v-if="props.isOpen" class="fixed inset-0 z-50 flex items-center justify-center p-4">
      <button
        type="button"
        aria-label="Close modal"
        class="absolute inset-0 bg-black/50 backdrop-blur-[2px] fade-in"
        @click="props.closeOnOverlay ? emit('close') : undefined"
      />
      <div
        ref="dialogRef"
        role="dialog"
        aria-modal="true"
        :aria-label="props.title"
        tabindex="-1"
        :class="cn(
          'relative w-full bg-surface outline-none',
          'border border-border-subtle',
          'rounded-[14px] shadow-elev',
          'fade-in',
          sizes[props.size],
          props.className,
        )"
      >
        <div v-if="props.title || props.showTrafficLights" class="flex items-center justify-between p-2 border-b border-border-subtle">
          <div class="flex items-center">
            <div v-if="props.showTrafficLights" class="hidden md:flex items-center gap-2 mr-4 ml-2">
              <Tooltip text="Close" position="top" color="#FF5F56">
                <button
                  type="button"
                  aria-label="Close"
                  title="Close"
                  class="w-4 h-4 rounded-full bg-[#FF5F56] hover:brightness-90 transition-all cursor-pointer flex items-center justify-center group/dot"
                  @click="emit('close')"
                >
                  <span class="text-[9px] font-bold text-white opacity-0 group-hover/dot:opacity-100 transition-opacity leading-none">✕</span>
                </button>
              </Tooltip>
              <div class="w-4 h-4 rounded-full bg-[#3a3a3a]/20 dark:bg-white/15 cursor-not-allowed" />
              <div class="w-4 h-4 rounded-full bg-[#3a3a3a]/20 dark:bg-white/15 cursor-not-allowed" />
            </div>
            <h2 v-if="props.title" class="text-lg font-semibold text-text-main">{{ props.title }}</h2>
          </div>
          <button
            type="button"
            aria-label="Close"
            class="md:hidden p-1.5 rounded-[10px] text-text-muted hover:bg-surface-2 hover:text-text-main transition-colors"
            @click="emit('close')"
          >
            <span class="material-symbols-outlined text-[20px]">close</span>
          </button>
        </div>
        <div class="p-6 max-h-[calc(85vh-100px)] overflow-y-auto custom-scrollbar">
          <slot />
        </div>
        <div v-if="$slots.footer" class="flex items-center justify-end gap-3 p-6 border-t border-border-subtle">
          <slot name="footer" />
        </div>
      </div>
    </div>
  </Teleport>
</template>
