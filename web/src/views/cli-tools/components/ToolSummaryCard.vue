<script setup lang="ts">
import { RouterLink } from "vue-router";

import Card from "@/components/ui/UiCard.vue";
import type { CliTool } from "@/constants/cliTools";

const props = defineProps<{
	toolId: string;
	tool: CliTool;
}>();
</script>

<template>
  <RouterLink :to="`/dashboard/cli-tools/${props.toolId}`" class="block">
    <Card padding="sm" class="h-full overflow-hidden hover:border-primary/50 transition-colors cursor-pointer">
      <div class="flex h-full flex-col gap-2">
        <div class="flex items-center gap-3">
          <div class="size-8 flex items-center justify-center shrink-0">
            <img
              v-if="props.tool.image"
              :src="props.tool.image"
              :alt="props.tool.name"
              width="32"
              height="32"
              class="size-8 object-contain rounded-lg"
              sizes="32px"
              loading="lazy"
              decoding="async"
              @error="($event.target as HTMLImageElement).style.display = 'none'"
            />
            <span
              v-else-if="props.tool.color"
              class="material-symbols-outlined text-[28px]"
              :style="{ color: props.tool.color }"
            />
          </div>
          <div class="min-w-0 flex-1">
            <h3 class="font-medium text-sm truncate">{{ props.tool.name }}</h3>
            <p class="text-xs text-text-muted truncate">{{ props.tool.description }}</p>
          </div>
          <span class="material-symbols-outlined text-text-muted text-[18px] shrink-0">chevron_right</span>
        </div>
      </div>
    </Card>
  </RouterLink>
</template>
