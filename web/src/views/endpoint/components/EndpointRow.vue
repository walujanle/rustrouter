<script setup lang="ts">
import Input from "@/components/ui/UiInput.vue";
import { cn } from "@/utils/cn";

/** Reusable endpoint row component */
withDefaults(
	defineProps<{
		label: string;
		url: string;
		copyId: string;
		copied?: string | null;
		badge?: string;
	}>(),
	{ copied: null },
);

const emit = defineEmits<{ copy: [url: string, copyId: string] }>();
</script>

<template>
  <div class="flex items-center gap-2">
    <span
      :class="cn(
        'text-xs font-mono px-1.5 py-0.5 rounded shrink-0 min-w-22 text-center',
        badge === 'CF' || badge === 'TS' ? 'bg-primary/10 text-primary' : 'bg-surface-2 text-text-muted',
      )"
    >{{ label }}</span>
    <Input :model-value="url" readonly class-name="flex-1 font-mono text-sm" />
    <button
      type="button"
      :aria-label="copied === copyId ? 'Copied' : 'Copy endpoint URL'"
      :title="copied === copyId ? 'Copied' : 'Copy endpoint URL'"
      class="p-2 hover:bg-black/5 dark:hover:bg-white/5 rounded text-text-muted hover:text-primary transition-colors shrink-0"
      @click="emit('copy', url, copyId)"
    >
      <span class="material-symbols-outlined text-[18px]">{{ copied === copyId ? "check" : "content_copy" }}</span>
    </button>
    <slot />
  </div>
</template>
