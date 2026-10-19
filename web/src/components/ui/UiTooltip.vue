<script setup lang="ts">
import { computed, ref } from "vue";

const props = withDefaults(
	defineProps<{
		text: string;
		position?: "top" | "bottom" | "left" | "right";
		color?: string;
	}>(),
	{ position: "top" },
);

/**
 * The bubble is teleported to `body` and positioned with `position: fixed`.
 * Rendering it inside the wrapper made every scroll container with
 * `overflow-y-auto` clip it, which is what `ConnectionsCard` does.
 */
const anchor = ref<HTMLElement | null>(null);
const visible = ref(false);
const coords = ref({ top: 0, left: 0 });

function place() {
	const el = anchor.value;
	if (!el) return;
	const rect = el.getBoundingClientRect();
	const gap = 6;
	switch (props.position) {
		case "bottom":
			coords.value = { top: rect.bottom + gap, left: rect.left + rect.width / 2 };
			break;
		case "left":
			coords.value = { top: rect.top + rect.height / 2, left: rect.left - gap };
			break;
		case "right":
			coords.value = { top: rect.top + rect.height / 2, left: rect.right + gap };
			break;
		default:
			coords.value = { top: rect.top - gap, left: rect.left + rect.width / 2 };
	}
}

function show() {
	place();
	visible.value = true;
}

function hide() {
	visible.value = false;
}

const transform = computed(() => {
	switch (props.position) {
		case "bottom":
			return "translate(-50%, 0)";
		case "left":
			return "translate(-100%, -50%)";
		case "right":
			return "translate(0, -50%)";
		default:
			return "translate(-50%, -100%)";
	}
});

const bgStyle = computed(() => (props.color ? { backgroundColor: props.color } : {}));
const bgClass = computed(() => (props.color ? "" : "bg-gray-900"));
</script>

<template>
  <!-- `focus-within` reveals the tooltip when the wrapped control is focused,
       so it is reachable by keyboard and not hover-only. The wrapper only
       observes pointer and focus events from the interactive control in its
       slot; it is not itself a control. -->
  <!-- biome-ignore lint/a11y/noStaticElementInteractions: the slot owns the interactive control. -->
  <div
    ref="anchor"
    class="relative inline-flex"
    @mouseenter="show"
    @mouseleave="hide"
    @focusin="show"
    @focusout="hide"
  >
    <slot />
  </div>
  <Teleport to="body">
    <div
      v-if="visible"
      role="tooltip"
      :class="`pointer-events-none fixed z-[60] w-max max-w-56 rounded px-2 py-1 text-[11px] leading-snug ${bgClass} text-white whitespace-normal shadow-lg`"
      :style="{
        top: `${coords.top}px`,
        left: `${coords.left}px`,
        transform,
        ...bgStyle,
      }"
    >
      {{ props.text }}
    </div>
  </Teleport>
</template>
