<script setup lang="ts">
import { computed, ref } from "vue";

import ModelSelectModal from "@/components/ModelSelectModal.vue";
import CapacityBadges from "@/components/ui/CapacityBadges.vue";
import Card from "@/components/ui/UiCard.vue";
import Select from "@/components/ui/UiSelect.vue";
import type { Combo, ModelCaps } from "../utils";
import { aggregateComboCapabilities, fmtK, STRATEGY_OPTIONS } from "../utils";

const props = withDefaults(
	defineProps<{
		combo: Combo;
		getCaps?: (key: string) => ModelCaps | null;
		comboByName?: Record<string, string[]>;
		activeProviders?: Array<Record<string, any>>;
		copied?: string | null;
		strategy?: { fallbackStrategy?: string; judgeModel?: string };
		selected?: boolean;
	}>(),
	{ comboByName: () => ({}), activeProviders: () => [], copied: null, strategy: () => ({}), selected: false },
);

const emit = defineEmits<{
	copy: [text: string, id: string];
	edit: [];
	delete: [];
	"toggle-select": [];
	"set-strategy": [patch: { fallbackStrategy?: string; judgeModel?: string }];
}>();

const showJudgeSelect = ref(false);

const current = computed(() => props.strategy.fallbackStrategy || "fallback");
const judge = computed(() => props.strategy.judgeModel || "");
const isFusion = computed(() => current.value === "fusion");

// The synced catalog is server-only, so resolving here would fall back to the
// generic patterns and under-report the limits. getCaps carries the server's
// answer for /api/models.
const comboCaps = computed(() =>
	aggregateComboCapabilities(props.combo.models, props.comboByName, props.getCaps),
);

function memberCaps(model: string): ModelCaps | null {
	return props.comboByName[model]
		? aggregateComboCapabilities(props.comboByName[model], props.comboByName, props.getCaps)
		: (props.getCaps?.(model) ?? null);
}

function onSelectJudge(m: { value?: string } | null) {
	emit("set-strategy", { judgeModel: m?.value || "" });
	showJudgeSelect.value = false;
}
</script>

<template>
  <Card padding="sm" :class="`group ${props.selected ? 'ring-1 ring-primary/40 bg-primary/3' : ''}`">
    <div class="flex min-w-0 flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
      <div class="flex min-w-0 flex-1 items-start gap-3 sm:items-center">
        <label class="flex shrink-0 items-center pt-1 sm:pt-0 cursor-pointer" title="Select combo">
          <input
            type="checkbox"
            :checked="props.selected"
            class="h-4 w-4 rounded border-gray-300 text-primary focus:ring-primary"
            :aria-label="`Select ${props.combo.name}`"
            @change="emit('toggle-select')"
            @click.stop
          />
          <span class="sr-only">Select combo</span>
        </label>
        <div class="size-8 rounded-lg bg-primary/10 flex items-center justify-center shrink-0">
          <span class="material-symbols-outlined text-primary text-[18px]">layers</span>
        </div>
        <div class="min-w-0 flex-1">
          <code class="block truncate font-mono text-sm font-medium">{{ props.combo.name }}</code>
          <div class="mt-1 flex min-w-0 flex-wrap items-center gap-1">
            <template v-if="props.combo.models.length === 0">
              <span class="text-xs text-text-muted italic">No models</span>
            </template>
            <template v-else>
              <code
                v-for="(model, index) in props.combo.models.slice(0, 3)"
                :key="index"
                class="inline-flex items-center gap-1 rounded bg-black/5 px-1.5 py-0.5 font-mono text-xs text-text-muted dark:bg-white/5"
              >
                <span>{{ model }}</span>
                <CapacityBadges :caps="memberCaps(model)" />
              </code>
            </template>
            <span v-if="props.combo.models.length > 3" class="text-[10px] text-text-muted">
              +{{ props.combo.models.length - 3 }} more
            </span>
          </div>
          <div v-if="comboCaps" class="mt-1 flex items-center gap-2 text-[10px] text-text-muted">
            <span>ctx {{ fmtK(comboCaps.contextWindow) }}</span>
            <span class="opacity-40">·</span>
            <span>max {{ fmtK(comboCaps.maxOutput) }}</span>
          </div>
          <!-- Fusion: judge picker (Auto = first model) -->
          <div v-if="isFusion" class="mt-2 flex min-w-0 flex-wrap items-center gap-1.5">
            <span class="text-[11px] font-medium text-text-muted">Judge</span>
            <button
              type="button"
              class="inline-flex max-w-full items-center gap-1 rounded border border-dashed border-primary/40 px-1.5 py-0.5 font-mono text-[11px] text-primary hover:border-primary hover:bg-primary/5 transition-colors"
              title="Pick the model that fuses panel answers"
              @click="showJudgeSelect = true"
            >
              <span class="material-symbols-outlined text-[13px]">gavel</span>
              <span class="truncate">{{ judge || `Auto — ${props.combo.models[0] || "first model"}` }}</span>
            </button>
            <button
              v-if="judge"
              type="button"
              class="p-0.5 rounded text-text-muted hover:text-red-500 hover:bg-red-500/10 transition-colors"
              title="Reset judge to Auto"
              @click="emit('set-strategy', { judgeModel: '' })"
            >
              <span class="material-symbols-outlined text-[13px]">close</span>
            </button>
          </div>
        </div>
      </div>

      <!-- Actions -->
      <div class="flex w-full flex-col gap-2 sm:w-auto sm:flex-row sm:items-center sm:gap-3 sm:shrink-0">
        <!-- Strategy selector — always visible -->
        <div class="w-full sm:w-50">
          <Select
            :options="STRATEGY_OPTIONS"
            :model-value="current"
            select-class-name="py-1.5 text-xs"
            @update:model-value="emit('set-strategy', { fallbackStrategy: $event })"
          />
        </div>

        <div class="grid grid-cols-3 gap-1 sm:flex">
          <button
            type="button"
            class="flex flex-col items-center rounded px-2 py-1 text-text-muted transition-colors hover:bg-black/5 hover:text-primary dark:hover:bg-white/5"
            title="Copy combo name"
            @click.stop="emit('copy', props.combo.name, `combo-${props.combo.id}`)"
          >
            <span class="material-symbols-outlined text-[18px]">
              {{ props.copied === `combo-${props.combo.id}` ? "check" : "content_copy" }}
            </span>
            <span class="text-[10px] leading-tight">Copy</span>
          </button>
          <button
            type="button"
            class="flex flex-col items-center rounded px-2 py-1 text-text-muted transition-colors hover:bg-black/5 hover:text-primary dark:hover:bg-white/5"
            title="Edit"
            @click="emit('edit')"
          >
            <span class="material-symbols-outlined text-[18px]">edit</span>
            <span class="text-[10px] leading-tight">Edit</span>
          </button>
          <button
            type="button"
            class="flex flex-col items-center rounded px-2 py-1 text-red-500 transition-colors hover:bg-red-500/10"
            title="Delete"
            @click="emit('delete')"
          >
            <span class="material-symbols-outlined text-[18px]">delete</span>
            <span class="text-[10px] leading-tight">Delete</span>
          </button>
        </div>
      </div>
    </div>

    <!-- Judge model picker (single-select; combo members make natural judges too) -->
    <ModelSelectModal
      v-if="showJudgeSelect"
      :is-open="showJudgeSelect"
      :active-providers="props.activeProviders"
      title="Select Judge Model"
      :added-model-values="judge ? [judge] : []"
      :close-on-select="true"
      @close="showJudgeSelect = false"
      @select="onSelectJudge"
    />
  </Card>
</template>
