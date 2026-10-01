<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";

import {
	deleteKeyPreset,
	readKeyPresets,
	subscribeKeyPresets,
	upsertKeyPreset,
} from "./cliEndpointPresets";

interface ApiKey {
	key: string;
}

const props = withDefaults(
	defineProps<{
		modelValue?: string;
		apiKeys?: ApiKey[];
		cloudEnabled?: boolean;
		className?: string;
	}>(),
	{ modelValue: "", apiKeys: () => [], cloudEnabled: false, className: "" },
);

const emit = defineEmits<{ "update:modelValue": [value: string] }>();

const CUSTOM_VALUE = "__custom__";
const SAVE_VALUE = "__save_key__";

interface Option {
	value: string;
	label: string;
	url?: string;
	saved?: boolean;
}

const savedKeys = ref<Array<{ name: string; key: string }>>([]);
// Custom mode is sticky once the user types, so an emptied input doesn't jump back to a dropdown option
const customMode = ref(false);
const customInput = ref("");

let unsubscribe: (() => void) | null = null;

onMounted(() => {
	const sync = () => {
		savedKeys.value = readKeyPresets();
	};
	sync();
	unsubscribe = subscribeKeyPresets(sync);
});

onBeforeUnmount(() => {
	unsubscribe?.();
});

const options = computed<Option[]>(() => [
	...props.apiKeys.map((k) => ({ value: k.key, label: k.key })),
	...savedKeys.value.map((p) => ({
		value: `saved:${p.name}`,
		label: p.key,
		url: p.key,
		saved: true,
	})),
	{ value: CUSTOM_VALUE, label: "Custom...", url: "" },
]);

// Derive the active option from value — no sync effects needed when the parent updates it
const matched = computed(() =>
	props.modelValue
		? options.value.find((o) => o.value === props.modelValue || o.url === props.modelValue) || null
		: null,
);
const mode = computed(() =>
	matched.value
		? matched.value.value
		: customMode.value || props.modelValue
			? CUSTOM_VALUE
			: (options.value[0]?.value ?? CUSTOM_VALUE),
);
const inputValue = computed(() => (customMode.value ? customInput.value : props.modelValue || ""));
const isSaved = computed(() => typeof mode.value === "string" && mode.value.startsWith("saved:"));
const isCustom = computed(() => mode.value === CUSTOM_VALUE);
const canSave = computed(
	() =>
		isCustom.value &&
		(props.modelValue || "").trim().length > 0 &&
		!props.apiKeys.some((k) => k.key === props.modelValue),
);
const noKeys = computed(
	() =>
		props.apiKeys.length === 0 &&
		savedKeys.value.length === 0 &&
		!customMode.value &&
		!props.modelValue,
);

function handleSelect(e: Event) {
	const next = (e.target as HTMLSelectElement).value;
	if (next === SAVE_VALUE) {
		upsertKeyPreset((props.modelValue || "").trim());
		return;
	}
	if (next === CUSTOM_VALUE) {
		customMode.value = true;
		customInput.value = "";
		emit("update:modelValue", "");
		return;
	}
	customMode.value = false;
	customInput.value = "";
	const opt = options.value.find((o) => o.value === next);
	if (opt) emit("update:modelValue", opt.url ?? opt.value);
}

function handleCustomInput(e: Event) {
	const v = (e.target as HTMLInputElement).value;
	customMode.value = true;
	customInput.value = v;
	emit("update:modelValue", v);
}

function handleDeleteSaved() {
	if (!isSaved.value) return;
	deleteKeyPreset(mode.value.slice(6));
	customMode.value = false;
	customInput.value = "";
	const fallback = options.value.find((o) => o.value !== CUSTOM_VALUE && o.value !== mode.value);
	emit("update:modelValue", fallback ? (fallback.url ?? fallback.value) : "");
}
</script>

<template>
  <span
    v-if="noKeys"
    :class="`min-w-0 rounded bg-surface/40 px-2 py-2 text-xs text-text-muted sm:py-1.5 ${props.className}`"
  >
    {{ props.cloudEnabled ? "No API keys - Create one in Keys page" : "sk_9router (default)" }}
  </span>

  <div v-else :class="`flex flex-col gap-1.5 ${props.className}`">
    <div class="flex items-center gap-2">
      <select
        :value="mode"
        class="flex-1 min-w-0 px-2 py-2 bg-surface rounded text-xs border border-border focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
        @change="handleSelect"
      >
        <option v-for="o in options" :key="o.value" :value="o.value">{{ o.label }}</option>
        <option v-if="canSave" :value="SAVE_VALUE">+ Save current as...</option>
      </select>
      <button
        v-if="isSaved"
        type="button"
        title="Delete saved key"
        class="p-1 text-text-muted hover:text-red-500 rounded transition-colors shrink-0"
        @click="handleDeleteSaved"
      >
        <span class="material-symbols-outlined text-[14px]">delete</span>
      </button>
    </div>
    <input
      v-if="isCustom"
      type="text"
      :value="inputValue"
      placeholder="sk-..."
      class="w-full min-w-0 px-2 py-2 bg-surface rounded border border-border text-xs focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
      @input="handleCustomInput"
    />
  </div>
</template>
