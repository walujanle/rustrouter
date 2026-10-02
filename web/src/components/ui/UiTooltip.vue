<script setup lang="ts">
import { computed } from "vue";

const props = withDefaults(
	defineProps<{
		text: string;
		position?: "top" | "bottom" | "left" | "right";
		color?: string;
	}>(),
	{ position: "top" },
);

const posClass = computed(
	() =>
		({
			top: "bottom-full left-1/2 -translate-x-1/2 mb-1.5",
			bottom: "top-full left-1/2 -translate-x-1/2 mt-1.5",
			left: "right-full top-1/2 -translate-y-1/2 mr-1.5",
			right: "left-full top-1/2 -translate-y-1/2 ml-1.5",
		})[props.position],
);

const bgStyle = computed(() => (props.color ? { backgroundColor: props.color } : {}));
const bgClass = computed(() => (props.color ? "" : "bg-gray-900"));
</script>

<template>
  <!-- `focus-within` reveals the tooltip when the wrapped control is focused,
       so it is reachable by keyboard and not hover-only. -->
  <div class="relative inline-flex group/tt">
    <slot />
    <div
      role="tooltip"
      :class="`pointer-events-none absolute ${posClass} z-50 w-max max-w-56 rounded px-2 py-1 text-[11px] leading-snug ${bgClass} text-white opacity-0 group-hover/tt:opacity-100 group-focus-within/tt:opacity-100 transition-opacity duration-150 whitespace-normal`"
      :style="bgStyle"
    >
      {{ props.text }}
    </div>
  </div>
</template>
