<script setup lang="ts">
import { computed } from "vue";

import CapacityBadges from "@/components/ui/CapacityBadges.vue";

const props = withDefaults(
	defineProps<{
		model: Record<string, any>;
		fullModel: string;
		alias?: string | null;
		copied?: string | null;
		testStatus?: "ok" | "error";
		isCustom?: boolean;
		isFree?: boolean;
		isTesting?: boolean;
		hasDisable?: boolean;
		hideTest?: boolean;
		caps?: Record<string, any> | null;
		thinkingSuffix?: string | null;
	}>(),
	{
		alias: null,
		copied: null,
		isCustom: false,
		isFree: false,
		isTesting: false,
		hasDisable: false,
		hideTest: false,
		caps: null,
		thinkingSuffix: null,
	},
);

const emit = defineEmits<{
	copy: [text: string, id: string];
	deleteAlias: [];
	test: [];
	disable: [];
}>();

// `(level)` suffix is appended to the copied/labelled name when a thinking
// level is picked.
const displayModel = computed(() =>
	props.thinkingSuffix ? `${props.fullModel}(${props.thinkingSuffix})` : props.fullModel,
);

const borderColor = computed(() =>
	props.testStatus === "ok"
		? "border-green-500/40"
		: props.testStatus === "error"
			? "border-red-500/40"
			: "border-border",
);

const iconColor = computed(() =>
	props.testStatus === "ok" ? "#22c55e" : props.testStatus === "error" ? "#ef4444" : undefined,
);

const iconName = computed(() =>
	props.testStatus === "ok" ? "check_circle" : props.testStatus === "error" ? "cancel" : "smart_toy",
);

const copyId = computed(() => `model-${props.model.id}`);
</script>

<template>
  <div :class="`group min-w-0 max-w-full rounded-lg border px-3 py-2 ${borderColor} hover:bg-sidebar/50`">
    <div class="flex min-w-0 items-start gap-2 sm:items-center">
      <span
        class="material-symbols-outlined shrink-0 text-base"
        :style="iconColor ? { color: iconColor } : undefined"
      >{{ iconName }}</span>
      <div class="flex min-w-0 flex-1 flex-col gap-1">
        <code class="max-w-[72vw] truncate rounded bg-sidebar px-1.5 py-0.5 font-mono text-xs text-text-muted sm:max-w-90">{{ displayModel }}</code>
        <span class="flex min-w-0 items-center text-[9px] gap-1 pl-1">
          <span v-if="props.model.name" class="truncate text-[9px] italic text-text-muted/70">{{ props.model.name }}</span>
          <CapacityBadges :caps="props.caps" color-override="text-text-muted/70" :size="12" />
        </span>
      </div>
      <div v-if="!props.hideTest" class="relative shrink-0 group/btn">
        <button
          type="button"
          :disabled="props.isTesting"
          :aria-label="props.isTesting ? 'Testing model' : `Test ${props.model.name}`"
          :title="props.isTesting ? 'Testing...' : 'Test'"
          :class="`rounded p-0.5 text-text-muted transition-opacity hover:bg-sidebar hover:text-primary ${props.isTesting ? 'opacity-100' : 'opacity-100 sm:opacity-0 sm:group-hover:opacity-100'}`"
          @click="emit('test')"
        >
          <span
            class="material-symbols-outlined text-sm"
            :style="props.isTesting ? { animation: 'spin 1s linear infinite' } : undefined"
          >{{ props.isTesting ? "progress_activity" : "science" }}</span>
        </button>
        <span class="pointer-events-none absolute mt-1 top-5 left-1/2 -translate-x-1/2 text-[10px] text-text-muted whitespace-nowrap opacity-0 group-hover/btn:opacity-100 transition-opacity">
          {{ props.isTesting ? "Testing..." : "Test" }}
        </span>
      </div>
      <div class="relative shrink-0 group/btn">
        <button
          type="button"
          :aria-label="props.copied === copyId ? 'Copied' : 'Copy model id'"
          :title="props.copied === copyId ? 'Copied' : 'Copy model id'"
          class="rounded p-0.5 text-text-muted hover:bg-sidebar hover:text-primary"
          @click="emit('copy', displayModel, copyId)"
        >
          <span class="material-symbols-outlined text-sm">{{ props.copied === copyId ? "check" : "content_copy" }}</span>
        </button>
        <span class="pointer-events-none absolute mt-1 top-5 left-1/2 -translate-x-1/2 text-[10px] text-text-muted whitespace-nowrap opacity-0 group-hover/btn:opacity-100 transition-opacity">
          {{ props.copied === copyId ? "Copied!" : "Copy" }}
        </span>
      </div>
      <span v-if="props.isFree" class="text-[10px] font-bold text-green-500 bg-green-500/10 px-1.5 py-0.5 rounded">FREE</span>
      <button
        v-if="props.isCustom"
        type="button"
        class="ml-auto rounded p-0.5 text-text-muted opacity-100 transition-opacity hover:bg-red-500/10 hover:text-red-500 sm:opacity-0 sm:group-hover:opacity-100"
        title="Remove custom model"
        @click="emit('deleteAlias')"
      >
        <span class="material-symbols-outlined text-sm">close</span>
      </button>
      <button
        v-else-if="props.hasDisable"
        type="button"
        class="ml-auto rounded p-0.5 text-text-muted opacity-100 transition-opacity hover:bg-red-500/10 hover:text-red-500 sm:opacity-0 sm:group-hover:opacity-100"
        title="Disable this model"
        @click="emit('disable')"
      >
        <span class="material-symbols-outlined text-sm">close</span>
      </button>
    </div>
  </div>
</template>
