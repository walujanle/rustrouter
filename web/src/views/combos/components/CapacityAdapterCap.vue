<script setup lang="ts">
import { ref, useId } from "vue";

import ModelSelectModal from "@/components/ModelSelectModal.vue";
import CapacityBadges from "@/components/ui/CapacityBadges.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import type { CapEntry, ModelCaps } from "../utils";
import { DEFAULT_FALLBACK_MODEL } from "../utils";

const props = defineProps<{
	cap: { key: string; label: string; icon: string; desc: string };
	entry: CapEntry;
	activeProviders: Array<Record<string, any>>;
	getCaps?: (key: string) => ModelCaps | null;
}>();

const emit = defineEmits<{ change: [entry: CapEntry] }>();

const showModelSelect = ref(false);

const uid = useId();

function patch(p: Partial<CapEntry>) {
	emit("change", { ...props.entry, ...p });
}

function handleAdd(model: { value?: string; name?: string } | string) {
	const value = typeof model === "string" ? model : model?.value || model?.name;
	if (!value || props.entry.models.includes(value)) return;
	patch({ models: [...props.entry.models, value] });
}

function handleDeselect(model: { value?: string; name?: string } | string) {
	const value = typeof model === "string" ? model : model?.value || model?.name;
	const next = props.entry.models.filter((m) => m !== value);
	patch({ models: next.length === 0 ? [DEFAULT_FALLBACK_MODEL] : next });
}

function handleRemove(index: number) {
	const next = props.entry.models.filter((_, i) => i !== index);
	patch({ models: next.length === 0 ? [DEFAULT_FALLBACK_MODEL] : next });
}

function handleMove(index: number, delta: number) {
	const target = index + delta;
	if (target < 0 || target >= props.entry.models.length) return;
	const next = [...props.entry.models];
	[next[index], next[target]] = [next[target], next[index]];
	patch({ models: next });
}
</script>

<template>
  <Card padding="sm" :class="`group ${!props.entry.enabled ? 'opacity-50' : ''}`">
    <div class="flex min-w-0 flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
      <!-- Master toggle + icon + label -->
      <div class="flex min-w-0 flex-1 items-start gap-2.5 sm:items-center">
        <Toggle
          :model-value="props.entry.enabled"
          @update:model-value="patch({ enabled: $event })"
        />
        <div class="size-8 rounded-lg bg-primary/10 flex items-center justify-center shrink-0">
          <span class="material-symbols-outlined text-primary text-[18px]">{{ props.cap.icon }}</span>
        </div>
        <div class="min-w-0 flex-1">
          <div class="flex items-center gap-1.5">
            <code class="font-mono text-sm font-medium">{{ props.cap.label }}</code>
            <span class="text-[10px] text-text-muted">— {{ props.cap.desc }}</span>
          </div>
        </div>
      </div>

      <!-- Actions: Round-robin toggle + Add Model -->
      <div class="flex w-full flex-col gap-2 sm:w-auto sm:flex-row sm:items-center sm:gap-3 sm:shrink-0">
        <label class="flex items-center gap-1.5 text-xs text-text-muted cursor-pointer select-none" :for="`round-${uid}`">
          <Toggle
            :id="`round-${uid}`"
            :model-value="props.entry.roundRobin"
            :disabled="!props.entry.enabled"
            @update:model-value="patch({ roundRobin: $event })"
          />
          <span>Round</span>
        </label>
        <Button
          icon="add"
          variant="ghost"
          size="sm"
          :disabled="!props.entry.enabled"
          :title="`Add ${props.cap.label} model`"
          @click="showModelSelect = true"
        >
          Add Model
        </Button>
      </div>
    </div>

    <!-- Model pool list/table -->
    <div
      v-if="props.entry.models.length === 0"
      class="mt-3 py-2 text-center text-xs text-text-muted italic"
    >
      No models in pool (will fallback to {{ DEFAULT_FALLBACK_MODEL }})
    </div>
    <div v-else class="mt-3 overflow-hidden rounded-lg border border-border/50">
      <table class="w-full text-left text-xs">
        <thead>
          <tr class="border-b border-border/40 bg-black/2 text-text-muted dark:bg-white/2">
            <th class="w-12 px-3 py-1.5 font-medium text-center">#</th>
            <th class="px-3 py-1.5 font-medium">Model</th>
            <th class="w-24 px-3 py-1.5 font-medium text-center">Order</th>
            <th class="w-12 px-3 py-1.5 font-medium text-right" />
          </tr>
        </thead>
        <tbody class="divide-y divide-border/30 font-mono">
          <tr
            v-for="(model, index) in props.entry.models"
            :key="`${model}-${index}`"
            class="hover:bg-black/2 dark:hover:bg-white/2 transition-colors"
          >
            <td class="px-3 py-2 text-center text-text-muted text-[11px] font-sans">#{{ index + 1 }}</td>
            <td class="px-3 py-2 text-text-main">
              <div class="flex items-center gap-1.5 flex-wrap">
                <span class="truncate">{{ model }}</span>
                <CapacityBadges :caps="props.getCaps?.(model)" />
                <span
                  v-if="model === DEFAULT_FALLBACK_MODEL"
                  class="rounded bg-emerald-500/10 px-1.5 py-0.5 font-sans text-[10px] font-medium text-emerald-600 dark:text-emerald-400"
                >
                  free default
                </span>
              </div>
            </td>
            <td class="px-3 py-2 text-center">
              <div class="inline-flex items-center gap-1">
                <button
                  type="button"
                  :disabled="!props.entry.enabled || index === 0"
                  :class="`p-1 rounded transition-colors ${
                    !props.entry.enabled || index === 0
                      ? 'text-text-muted/20 cursor-not-allowed'
                      : 'text-text-muted hover:text-primary hover:bg-black/5 dark:hover:bg-white/5'
                  }`"
                  title="Move up"
                  @click="handleMove(index, -1)"
                >
                  <span class="material-symbols-outlined text-[16px] leading-none">arrow_upward</span>
                </button>
                <button
                  type="button"
                  :disabled="!props.entry.enabled || index === props.entry.models.length - 1"
                  :class="`p-1 rounded transition-colors ${
                    !props.entry.enabled || index === props.entry.models.length - 1
                      ? 'text-text-muted/20 cursor-not-allowed'
                      : 'text-text-muted hover:text-primary hover:bg-black/5 dark:hover:bg-white/5'
                  }`"
                  title="Move down"
                  @click="handleMove(index, 1)"
                >
                  <span class="material-symbols-outlined text-[16px] leading-none">arrow_downward</span>
                </button>
              </div>
            </td>
            <td class="px-3 py-2 text-right">
              <button
                type="button"
                :disabled="!props.entry.enabled"
                :class="`p-1 rounded transition-colors ${
                  !props.entry.enabled
                    ? 'text-text-muted/20 cursor-not-allowed'
                    : 'text-text-muted hover:text-red-500 hover:bg-red-500/10'
                }`"
                title="Remove model"
                @click="handleRemove(index)"
              >
                <span class="material-symbols-outlined text-[16px] leading-none">close</span>
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <ModelSelectModal
      v-if="showModelSelect"
      :is-open="showModelSelect"
      :active-providers="props.activeProviders"
      :title="`Add ${props.cap.label} Model`"
      :added-model-values="props.entry.models"
      :cap-filter="props.cap.key"
      :close-on-select="false"
      @close="showModelSelect = false"
      @select="handleAdd"
      @deselect="handleDeselect"
    />
  </Card>
</template>
