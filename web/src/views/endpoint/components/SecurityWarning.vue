<script setup lang="ts">
interface SecurityAction {
	label: string;
	href: string;
}

const props = defineProps<{ message: string; action?: SecurityAction }>();

function handleActionClick(e: MouseEvent) {
	const href = props.action?.href;
	if (!href?.startsWith("#")) return;
	e.preventDefault();
	document.getElementById(href.slice(1))?.scrollIntoView({ behavior: "smooth" });
}
</script>

<template>
  <div class="flex items-center gap-2 px-3 py-2 rounded-lg bg-amber-500/10 border border-amber-500/20 text-amber-700 dark:text-amber-400">
    <span class="material-symbols-outlined text-[16px] shrink-0 mt-0.5">warning</span>
    <p class="text-xs flex-1">{{ props.message }}</p>
    <a
      v-if="props.action"
      :href="props.action.href"
      class="text-xs font-medium underline shrink-0 hover:opacity-80"
      @click="handleActionClick"
    >
      {{ props.action.label }}
    </a>
  </div>
</template>
