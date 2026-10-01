<script setup lang="ts">
import { computed } from "vue";
import { RouterLink } from "vue-router";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Badge from "@/components/ui/UiBadge.vue";
import Card from "@/components/ui/UiCard.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import {
	ANTHROPIC_COMPATIBLE_PREFIX,
	OPENAI_COMPATIBLE_PREFIX,
} from "@/constants/providers";
import { getProviderIconSrc } from "@/utils/providerIcon";

const props = withDefaults(
	defineProps<{
		providerId: string;
		provider: Record<string, any>;
		stats: Record<string, any>;
		apiKey?: boolean;
	}>(),
	{ apiKey: false },
);

const emit = defineEmits<{ toggle: [active: boolean] }>();

const isCompatible = computed(() =>
	props.providerId.startsWith(OPENAI_COMPATIBLE_PREFIX),
);
const isAnthropicCompatible = computed(() =>
	props.providerId.startsWith(ANTHROPIC_COMPATIBLE_PREFIX),
);
const isNoAuth = computed(() => !!props.provider.noAuth);

const iconSrc = computed<string | null>(() => {
	if (!props.apiKey) return `/providers/${props.provider.id}.png`;
	if (isCompatible.value && props.provider.apiType) {
		return props.provider.apiType === "responses"
			? "/providers/oai-r.png"
			: "/providers/oai-cc.png";
	}
	if (isAnthropicCompatible.value) return "/providers/anthropic-m.png";
	return getProviderIconSrc(props.provider.id);
});

const iconClass = computed(() =>
	props.apiKey
		? "object-contain rounded-lg max-w-7.5 max-h-7.5"
		: "object-contain rounded-lg max-w-8 max-h-8",
);

const fallbackText = computed(
	() => props.provider.textIcon || props.provider.id.slice(0, 2).toUpperCase(),
);

const wrapperStyle = computed(() => {
	const color = props.provider.color ?? "";
	return {
		backgroundColor: color.length > 7 ? color : `${color}15`,
	};
});

// `Toggle` already emits the new value (`!modelValue`), and `modelValue` is
// `!stats.allDisabled`, so forwarding it gives exactly `stats.allDisabled` —
// the new `isActive` the parent's handler expects. Recomputing it from the prop
// here would re-send the current state and make the toggle a no-op.
function handleToggle(active: boolean) {
	emit("toggle", active);
}
</script>

<template>
  <div class="group relative min-w-0">
    <RouterLink :to="`/dashboard/providers/${props.providerId}`" class="min-w-0">
      <Card
        padding="xs"
        :class="`h-full hover:bg-black/1 dark:hover:bg-white/1 transition-colors cursor-pointer ${props.stats.allDisabled ? 'opacity-50' : ''}`"
      >
        <div class="flex min-w-0 items-center justify-between gap-3" :class="props.stats.total > 0 ? 'pr-8' : ''">
          <div class="flex min-w-0 items-center gap-3">
            <div
              class="size-8 shrink-0 rounded-lg flex items-center justify-center"
              :style="wrapperStyle"
            >
              <ProviderIcon
                :src="iconSrc ?? undefined"
                :alt="props.provider.name"
                :size="30"
                :class="iconClass"
                :fallback-text="fallbackText"
                :fallback-color="props.provider.color"
              />
            </div>
            <div class="min-w-0">
              <h3 class="truncate font-semibold">{{ props.provider.name }}</h3>
              <div class="flex min-w-0 items-center gap-1.5 text-xs flex-wrap">
              <template v-if="props.stats.allDisabled">
                <Badge variant="default" size="sm">
                  <span class="flex items-center gap-1">
                    <span class="material-symbols-outlined text-[12px]">pause_circle</span>
                    Disabled
                  </span>
                </Badge>
              </template>
              <template v-else-if="!props.apiKey && isNoAuth">
                <Badge variant="success" size="sm" dot>Ready</Badge>
              </template>
              <template v-else>
                <Badge v-if="props.stats.connected > 0" variant="success" size="sm" dot>
                  {{ props.stats.connected }} Connected
                </Badge>
                <Badge v-if="props.stats.error > 0" variant="error" size="sm" dot>
                  {{ props.stats.error }} Error{{ props.stats.errorCode ? ` (${props.stats.errorCode})` : "" }}
                </Badge>
                <span
                  v-if="props.stats.connected === 0 && props.stats.error === 0"
                  class="text-text-muted"
                >No connections</span>
                <Badge v-if="props.apiKey && isCompatible" variant="default" size="sm">
                  {{ props.provider.apiType === "responses" ? "Responses" : "Chat" }}
                </Badge>
                <Badge v-if="props.apiKey && isAnthropicCompatible" variant="default" size="sm">
                  Messages
                </Badge>
                <span v-if="props.stats.errorTime" class="text-text-muted">{{ props.stats.errorTime }}</span>
              </template>
              </div>
            </div>
          </div>
        </div>
      </Card>
    </RouterLink>
    <!-- Outside the link: a button nested in an anchor is invalid markup and
         gives the toggle no keyboard path of its own. -->
    <div
      v-if="props.stats.total > 0"
      class="absolute right-3 top-1/2 -translate-y-1/2 opacity-100 transition-opacity sm:opacity-0 sm:group-hover:opacity-100 sm:group-focus-within:opacity-100"
    >
      <Toggle
        size="sm"
        :model-value="!props.stats.allDisabled"
        :aria-label="props.stats.allDisabled ? 'Enable provider' : 'Disable provider'"
        :title="props.stats.allDisabled ? 'Enable provider' : 'Disable provider'"
        @update:model-value="handleToggle"
      />
    </div>
  </div>
</template>
