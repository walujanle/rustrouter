<script setup lang="ts">
import type { CapEntry, ModelCaps } from "../utils";
import { CAPACITY_ADAPTER_CAPS, EMPTY_CAP_ENTRY } from "../utils";
import CapacityAdapterCap from "./CapacityAdapterCap.vue";

const props = defineProps<{
	capacityAdapter: Record<string, CapEntry>;
	activeProviders: Array<Record<string, any>>;
	getCaps?: (key: string) => ModelCaps | null;
}>();

const emit = defineEmits<{ change: [next: Record<string, CapEntry>] }>();

function setEntry(key: string, entry: CapEntry) {
	emit("change", { ...props.capacityAdapter, [key]: entry });
}
</script>

<template>
  <div class="flex flex-col gap-3">
    <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
      <div class="min-w-0">
        <p class="text-sm font-medium">Vision Adapter</p>
        <p class="text-xs text-text-muted mt-0.5">
          Your model can&apos;t read image/audio? Auto-switches to a model in the pool below.
        </p>
      </div>
    </div>
    <div class="flex flex-col gap-4">
      <CapacityAdapterCap
        v-for="cap in CAPACITY_ADAPTER_CAPS"
        :key="cap.key"
        :cap="cap"
        :entry="props.capacityAdapter[cap.key] || EMPTY_CAP_ENTRY"
        :active-providers="props.activeProviders"
        :get-caps="props.getCaps"
        @change="setEntry(cap.key, $event)"
      />
    </div>
  </div>
</template>
