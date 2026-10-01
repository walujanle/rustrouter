<script setup lang="ts">
import Skeleton from "./UiSkeleton.vue";
</script>

<template>
  <div class="p-6 rounded-[14px] border border-border-subtle bg-surface shadow-soft">
    <div class="flex items-center justify-between mb-4">
      <Skeleton class="h-4 w-24" />
      <Skeleton class="size-10 rounded-[10px]" />
    </div>
    <Skeleton class="h-8 w-16 mb-2" />
    <Skeleton class="h-3 w-20" />
  </div>
</template>
