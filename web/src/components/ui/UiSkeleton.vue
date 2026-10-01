<script setup lang="ts">
import { cn } from "@/utils/cn";

const props = defineProps<{ className?: string }>();
</script>

<template>
  <div :class="cn('animate-pulse rounded-[10px] bg-surface-2', props.className)" />
</template>
