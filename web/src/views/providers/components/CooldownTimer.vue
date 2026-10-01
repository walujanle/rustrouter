<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";

const props = defineProps<{ until: string }>();

const remaining = ref("");
let timer: ReturnType<typeof setInterval> | null = null;

function update() {
	const diff = new Date(props.until).getTime() - Date.now();
	if (diff <= 0) {
		remaining.value = "";
		return;
	}
	const s = Math.floor(diff / 1000);
	if (s < 60) remaining.value = `${s}s`;
	else if (s < 3600) remaining.value = `${Math.floor(s / 60)}m ${s % 60}s`;
	else
		remaining.value = `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

onMounted(() => {
	update();
	timer = setInterval(update, 1000);
});

onBeforeUnmount(() => {
	if (timer) clearInterval(timer);
});
</script>

<template>
  <span v-if="remaining" class="text-xs text-orange-500 font-mono">⏱ {{ remaining }}</span>
</template>
