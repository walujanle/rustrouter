<script setup lang="ts">
import { cn } from "@/utils/cn";

const props = defineProps<{ className?: string }>();
</script>

<template>
  <div :class="cn('p-4 rounded-[10px]', 'bg-bg border border-border-subtle', props.className)">
    <slot />
  </div>
</template>
