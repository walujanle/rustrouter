<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import { liveAppPort } from "@/constants/config";
import {
	deletePreset,
	readPresets,
	stripSlash,
	subscribePresets,
	upsertPreset,
} from "./cliEndpointPresets";

const props = withDefaults(
	defineProps<{
		modelValue?: string;
		requiresExternalUrl?: boolean;
		withV1?: boolean;
		currentUrl?: string;
	}>(),
	{ modelValue: "", requiresExternalUrl: false, withV1: true, currentUrl: "" },
);

const emit = defineEmits<{ "update:modelValue": [value: string] }>();

const CUSTOM_VALUE = "__custom__";
const SAVE_VALUE = "__save__";

interface Option {
	value: string;
	label: string;
	url: string;
	saved?: boolean;
}

const ensureV1 = (url: string) => {
	const trimmed = (url || "").replace(/\/+$/, "");
	if (!trimmed) return "";
	return /\/v1$/.test(trimmed) ? trimmed : `${trimmed}/v1`;
};

const savedPresets = ref<Array<{ name: string; baseUrl: string }>>([]);
const presetsLoaded = ref(false);
const mode = ref("");
const customInput = ref("");
const customInputRef = ref("");

function buildOptions(): Option[] {
	const opts: Option[] = [];
	const withV1 = props.withV1;
	const wrap = (url: string) => (withV1 ? ensureV1(url) : (url || "").replace(/\/+$/, ""));
	if (!props.requiresExternalUrl) {
		const localUrl = wrap(`http://127.0.0.1:${liveAppPort()}`);
		opts.push({ value: "local", label: localUrl, url: localUrl });
	}
	savedPresets.value.forEach((p) => {
		opts.push({ value: `saved:${p.name}`, label: p.baseUrl, url: p.baseUrl, saved: true });
	});
	opts.push({ value: CUSTOM_VALUE, label: "Custom URL...", url: "" });
	return opts;
}

const options = computed<Option[]>(() => buildOptions());

let unsubscribe: (() => void) | null = null;

onMounted(() => {
	const sync = () => {
		const presets = readPresets();
		savedPresets.value = presets;
		// A preset saved elsewhere (e.g. on Apply) takes over the custom slot
		if (mode.value !== CUSTOM_VALUE) return;
		const typed = stripSlash(customInputRef.value);
		if (!typed) return;
		const match = presets.find((p) => {
			const saved = stripSlash(p.baseUrl);
			return saved === typed || saved === ensureV1(typed);
		});
		if (match) mode.value = `saved:${match.name}`;
	};
	sync();
	presetsLoaded.value = true;
	unsubscribe = subscribePresets(sync);
});

onBeforeUnmount(() => {
	unsubscribe?.();
});

const initialized = ref(false);
const currentUrlRef = ref("");

// Sync the active config URL without replacing edits unless the config itself changes.
watch(
	[presetsLoaded, options, () => props.currentUrl, () => props.withV1],
	() => {
		if (!presetsLoaded.value || options.value.length === 0) return;
		const normalizeUrl = (url: string) => (props.withV1 ? ensureV1(url) : stripSlash(url));
		const current = normalizeUrl(props.currentUrl);
		if (initialized.value && currentUrlRef.value === current) return;
		initialized.value = true;
		currentUrlRef.value = current;
		const matched = current
			? options.value.find((o) => o.value !== CUSTOM_VALUE && normalizeUrl(o.url) === current)
			: null;
		if (matched) {
			customInput.value = "";
			customInputRef.value = "";
			mode.value = matched.value;
			emit("update:modelValue", matched.url);
		} else if (current) {
			customInput.value = current;
			customInputRef.value = current;
			mode.value = CUSTOM_VALUE;
			emit("update:modelValue", current);
		} else {
			const target = options.value.find((o) => o.value !== CUSTOM_VALUE);
			if (!target) return;
			mode.value = target.value;
			emit("update:modelValue", target.url);
		}
	},
);

function handleSelect(e: Event) {
	const next = (e.target as HTMLSelectElement).value;
	if (next === SAVE_VALUE) {
		const trimmed = (props.modelValue || "").trim();
		if (!trimmed) return;
		let defaultName = trimmed;
		try {
			defaultName = new URL(trimmed).host;
		} catch {}
		const name = window.prompt("Save endpoint as:", defaultName);
		const saved = name?.trim() ? upsertPreset(trimmed, name.trim()) : null;
		if (saved) mode.value = `saved:${saved}`;
		return;
	}
	mode.value = next;
	if (next === CUSTOM_VALUE) {
		customInput.value = "";
		emit("update:modelValue", "");
		return;
	}
	const opt = options.value.find((o) => o.value === next);
	if (opt) emit("update:modelValue", opt.url);
}

function handleCustomInput(e: Event) {
	const v = (e.target as HTMLInputElement).value;
	customInputRef.value = v;
	customInput.value = v;
	emit("update:modelValue", v);
}

function handleDeleteSaved() {
	if (!mode.value.startsWith("saved:")) return;
	deletePreset(mode.value.slice(6));
	customInput.value = "";
	const fallback = options.value.find(
		(o) => o.value !== CUSTOM_VALUE && o.value !== mode.value,
	);
	if (fallback) {
		mode.value = fallback.value;
		emit("update:modelValue", fallback.url);
	} else {
		mode.value = CUSTOM_VALUE;
		emit("update:modelValue", "");
	}
}

const isSaved = computed(() => mode.value.startsWith("saved:"));
const isCustom = computed(() => mode.value === CUSTOM_VALUE);
const canSave = computed(() => isCustom.value && (customInput.value || "").trim().length > 0);
</script>

<template>
  <div class="flex flex-col gap-1.5">
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
        title="Delete saved endpoint"
        class="p-1 text-text-muted hover:text-red-500 rounded transition-colors shrink-0"
        @click="handleDeleteSaved"
      >
        <span class="material-symbols-outlined text-[14px]">delete</span>
      </button>
    </div>
    <input
      v-if="isCustom"
      type="text"
      :value="customInput"
      :placeholder="props.withV1 ? 'https://example.com/v1' : 'https://example.com'"
      class="w-full min-w-0 px-2 py-2 bg-surface rounded border border-border text-xs focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
      @input="handleCustomInput"
    />
  </div>
</template>
