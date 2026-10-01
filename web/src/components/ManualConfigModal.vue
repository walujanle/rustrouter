<script setup lang="ts">
import { ref } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Modal from "@/components/ui/UiModal.vue";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";

interface ManualConfig {
	filename: string;
	content: string;
}

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		title?: string;
		configs?: ManualConfig[];
	}>(),
	{ title: "Manual Configuration", configs: () => [] },
);

const emit = defineEmits<{ close: [] }>();

const { copy } = useCopyToClipboard();
const copiedIndex = ref<number | null>(null);

function copyConfig(text: string, index: number) {
	copy(text, `manualconfig-${index}`);
	copiedIndex.value = index;
	setTimeout(() => {
		copiedIndex.value = null;
	}, 2000);
}
</script>

<template>
  <Modal :is-open="props.isOpen" :title="props.title" size="xl" @close="emit('close')">
    <div class="flex flex-col gap-4">
      <div v-for="(config, index) in props.configs" :key="index" class="flex flex-col gap-2">
        <div class="flex items-center justify-between">
          <span class="text-sm font-medium text-text-main">{{ config.filename }}</span>
          <Button variant="ghost" size="sm" @click="copyConfig(config.content, index)">
            <span class="material-symbols-outlined text-[14px] mr-1">
              {{ copiedIndex === index ? "check" : "content_copy" }}
            </span>
            {{ copiedIndex === index ? "Copied!" : "Copy" }}
          </Button>
        </div>
        <pre class="px-3 py-2 bg-black/5 dark:bg-white/5 rounded font-mono text-xs overflow-x-auto whitespace-pre-wrap break-all max-h-60 overflow-y-auto border border-border">{{ config.content }}</pre>
      </div>
    </div>
  </Modal>
</template>
