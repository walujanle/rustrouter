<script setup lang="ts">
// Label + control row used by the example cards.
defineProps<{ label: string }>();
</script>

<template>
  <div class="flex min-w-0 flex-col gap-1.5 sm:flex-row sm:items-center sm:gap-3">
    <span class="w-full text-xs font-medium text-text-muted sm:w-20 sm:shrink-0">{{ label }}</span>
    <div class="w-full min-w-0 flex-1">
      <slot />
    </div>
  </div>
</template>
