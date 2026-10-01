<script setup lang="ts">
import { computed, ref, watch } from "vue";

import { getProviderIconSrc, markProviderIconMissing } from "@/utils/providerIcon";

const props = withDefaults(
	defineProps<{
		src?: string;
		providerId?: string;
		alt?: string;
		size?: number;
		className?: string;
		fallbackText?: string;
		fallbackColor?: string;
	}>(),
	{ size: 32, className: "", fallbackText: "?" },
);

const errored = ref(false);

function resolveSrc(src?: string, providerId?: string): string | null {
	if (providerId) return getProviderIconSrc(providerId);
	if (!src) return null;
	const m = String(src).match(/^\/providers\/([^/]+)\.png$/i);
	if (m) return getProviderIconSrc(m[1]);
	return src;
}

const effectiveSrc = computed(() => resolveSrc(props.src, props.providerId));

// A changed source is a fresh attempt.
watch(effectiveSrc, () => {
	errored.value = false;
});

function onError() {
	if (effectiveSrc.value) {
		const m = effectiveSrc.value.match(/^\/providers\/([^/]+)\.png$/i);
		if (m) markProviderIconMissing(m[1]);
	}
	if (props.providerId) markProviderIconMissing(props.providerId);
	errored.value = true;
}
</script>

<template>
  <span
    v-if="!effectiveSrc || errored"
    class="inline-flex items-center justify-center font-bold rounded-lg"
    :class="props.className"
    :style="{
      width: `${props.size}px`,
      height: `${props.size}px`,
      color: props.fallbackColor,
      fontSize: `${Math.max(10, Math.floor(props.size * 0.38))}px`,
    }"
  >
    {{ props.fallbackText }}
  </span>
  <img
    v-else
    :src="effectiveSrc"
    :alt="props.alt"
    :width="props.size"
    :height="props.size"
    :class="props.className"
    loading="lazy"
    decoding="async"
    @error="onError"
  />
</template>
