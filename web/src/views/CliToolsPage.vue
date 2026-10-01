<script setup lang="ts">
import { computed } from "vue";

import { CLI_TOOLS } from "@/constants/cliTools";
import ToolSummaryCard from "./cli-tools/components/ToolSummaryCard.vue";

const regularTools = computed(() => Object.entries(CLI_TOOLS));
</script>

<template>
  <div class="mx-auto flex w-full max-w-5xl flex-col gap-6 px-1 sm:px-0">
    <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3 sm:gap-4">
      <ToolSummaryCard
        v-for="[toolId, tool] in regularTools"
        :key="toolId"
        :tool-id="toolId"
        :tool="tool"
      />
    </div>
  </div>
</template>
