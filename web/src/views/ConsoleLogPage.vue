<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import { CONSOLE_LOG_CONFIG } from "@/constants/config";

const LOG_LEVEL_COLORS: Record<string, string> = {
	LOG: "text-green-400",
	INFO: "text-blue-400",
	WARN: "text-yellow-400",
	ERROR: "text-red-400",
	DEBUG: "text-purple-400",
};

function lineColor(line: string): string {
	const match = line.match(/\[(\w+)\]/g);
	const levelTag = match ? match[1]?.replace(/\[|\]/g, "") : null;
	return LOG_LEVEL_COLORS[levelTag ?? ""] || "text-green-400";
}

const logs = ref<string[]>([]);
const connected = ref(false);
const logRef = ref<HTMLElement | null>(null);
let source: EventSource | null = null;

async function handleClear() {
	try {
		await fetch("/api/translator/console-logs", { method: "DELETE" });
		// UI cleared via SSE "clear" event
	} catch (err) {
		console.error("Failed to clear console logs:", err);
	}
}

onMounted(() => {
	const es = new EventSource("/api/translator/console-logs/stream");
	source = es;

	es.onopen = () => {
		connected.value = true;
	};

	es.onmessage = (e) => {
		const msg = JSON.parse(e.data);
		if (msg.type === "init") {
			logs.value = msg.logs.slice(-CONSOLE_LOG_CONFIG.maxLines);
		} else if (msg.type === "line") {
			const next = [...logs.value, msg.line];
			logs.value =
				next.length > CONSOLE_LOG_CONFIG.maxLines
					? next.slice(-CONSOLE_LOG_CONFIG.maxLines)
					: next;
		} else if (msg.type === "lines") {
			const next = [...logs.value, ...msg.lines];
			logs.value =
				next.length > CONSOLE_LOG_CONFIG.maxLines
					? next.slice(-CONSOLE_LOG_CONFIG.maxLines)
					: next;
		} else if (msg.type === "clear") {
			logs.value = [];
		}
	};

	es.onerror = () => {
		connected.value = false;
	};
});

onBeforeUnmount(() => {
	source?.close();
});

// Auto-scroll to bottom on new logs
watch(logs, () => {
	if (!logRef.value) return;
	logRef.value.scrollTop = logRef.value.scrollHeight;
});
</script>

<template>
  <div>
    <Card>
      <div class="flex items-center justify-between px-4 pt-3 pb-2">
        <span
          class="inline-flex items-center gap-1.5 text-xs"
          :class="connected ? 'text-green-600 dark:text-green-400' : 'text-red-600 dark:text-red-400'"
        >
          <span class="size-2 rounded-full" :class="connected ? 'bg-green-500' : 'bg-red-500'" />
          {{ connected ? "Connected" : "Disconnected" }}
        </span>
        <Button size="sm" variant="outline" icon="delete" @click="handleClear">
          Clear
        </Button>
      </div>
      <div
        ref="logRef"
        class="bg-black rounded-b-lg p-4 text-xs font-mono h-[calc(100vh-220px)] overflow-y-auto"
      >
        <span v-if="logs.length === 0 && connected" class="text-text-muted">No console logs yet.</span>
        <span v-else-if="logs.length === 0" class="text-red-400">
          Log stream disconnected. Reload the page to reconnect.
        </span>
        <div v-else class="space-y-0.5">
          <div v-for="(line, i) in logs" :key="i">
            <span :class="lineColor(line)">{{ line }}</span>
          </div>
        </div>
      </div>
    </Card>
  </div>
</template>
