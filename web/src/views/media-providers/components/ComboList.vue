<script setup lang="ts">
import { computed } from "vue";
import { RouterLink } from "vue-router";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Card from "@/components/ui/UiCard.vue";
import { useProviders } from "@/constants/providers";

// `emptyText` unset means "render nothing when empty" (the [kind] listing); the
// /web page passes one so each section shows a placeholder.

const props = withDefaults(
	defineProps<{ combos: Array<Record<string, any>>; emptyText?: string }>(),
	{ emptyText: "" },
);

const { AI_PROVIDERS } = useProviders();

function entryProvider(entry: unknown): { id: string; provider: Record<string, any> | null } {
	const id = typeof entry === "string" ? entry.split("/")[0] : "";
	return { id, provider: AI_PROVIDERS[id] ?? null };
}

const showEmpty = computed(() => props.combos.length === 0 && !!props.emptyText);
</script>

<template>
  <p v-if="showEmpty" class="text-xs text-text-muted italic">{{ emptyText }}</p>
  <div v-else-if="combos.length > 0" class="flex flex-col gap-2">
    <RouterLink v-for="combo in combos" :key="combo.id" :to="`/dashboard/media-providers/combo/${combo.id}`">
      <Card padding="xs" class="hover:bg-black/[0.02] dark:hover:bg-white/[0.02] transition-colors cursor-pointer">
        <div class="flex min-w-0 items-center gap-3">
          <span class="material-symbols-outlined text-primary text-[18px]">layers</span>
          <code class="text-sm font-mono font-medium flex-1 truncate">{{ combo.name }}</code>
          <div class="flex flex-wrap items-center gap-1 sm:shrink-0">
            <div
              v-for="(entry, i) in combo.models.slice(0, 6)"
              :key="`${entry}-${i}`"
              class="size-5 rounded flex items-center justify-center"
              :title="entryProvider(entry).provider?.name || entry"
              :style="{ backgroundColor: `${entryProvider(entry).provider?.color ?? '#888'}15` }"
            >
              <ProviderIcon
                :src="`/providers/${entryProvider(entry).id}.png`"
                :alt="entryProvider(entry).provider?.name || entryProvider(entry).id"
                :size="18"
                class="object-contain rounded max-w-[18px] max-h-[18px]"
                :fallback-text="entryProvider(entry).provider?.textIcon || entryProvider(entry).id.slice(0, 2).toUpperCase()"
                :fallback-color="entryProvider(entry).provider?.color"
              />
            </div>
            <span v-if="combo.models.length > 6" class="text-[10px] text-text-muted ml-1">
              +{{ combo.models.length - 6 }}
            </span>
          </div>
          <span class="text-[11px] text-text-muted shrink-0">{{ combo.models.length }}</span>
          <span class="material-symbols-outlined text-text-muted text-[16px]">chevron_right</span>
        </div>
      </Card>
    </RouterLink>
  </div>
</template>
