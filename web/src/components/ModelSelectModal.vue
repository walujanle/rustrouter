<script setup lang="ts">
import { computed, ref, watch } from "vue";

import CapacityBadges from "@/components/ui/CapacityBadges.vue";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Modal from "@/components/ui/UiModal.vue";
import { getModelKind, useModels } from "@/constants/models";
import {
	isAnthropicCompatibleProvider,
	isOpenAICompatibleProvider,
	useProviders,
} from "@/constants/providers";
import { useModelCaps } from "@/hooks/useModelCaps";

interface ActiveProvider {
	id?: string;
	provider?: string;
	name?: string;
	providerSpecificData?: Record<string, any>;
}

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		selectedModel?: string;
		activeProviders?: ActiveProvider[];
		title?: string;
		modelAliases?: Record<string, string>;
		kindFilter?: string | null;
		capFilter?: string | null;
		addedModelValues?: string[];
		closeOnSelect?: boolean;
	}>(),
	{
		title: "Select Model",
		modelAliases: () => ({}),
		kindFilter: null,
		capFilter: null,
		activeProviders: () => [],
		addedModelValues: () => [],
		closeOnSelect: true,
	},
);

const emit = defineEmits<{ close: []; select: [model: any]; deselect: [model: any] }>();

const { AI_PROVIDERS, FREE_PROVIDERS, FREE_TIER_PROVIDERS, OAUTH_PROVIDERS, APIKEY_PROVIDERS, getProviderAlias } =
	useProviders();
const { getModelsByProviderId } = useModels();
const { getCaps } = useModelCaps();

// Provider order: OAuth first, then Free Tier, then API Key (matches dashboard/providers)
const PROVIDER_ORDER = [
	...Object.keys(OAUTH_PROVIDERS),
	...Object.keys(FREE_PROVIDERS),
	...Object.keys(FREE_TIER_PROVIDERS),
	...Object.keys(APIKEY_PROVIDERS),
];

// Providers that need no auth — always show in the model selector
const NO_AUTH_PROVIDER_IDS = Object.keys(FREE_PROVIDERS).filter((id) => FREE_PROVIDERS[id].noAuth);

const searchQuery = ref("");
const combos = ref<Array<Record<string, any>>>([]);
const providerNodes = ref<Array<Record<string, any>>>([]);
const customModels = ref<Array<Record<string, any>>>([]);
const disabledModels = ref<Record<string, string[]>>({});

async function fetchCombos() {
	try {
		const res = await fetch("/api/combos");
		if (!res.ok) throw new Error(`Failed to fetch combos: ${res.status}`);
		const data = await res.json();
		combos.value = data.combos || [];
	} catch (error) {
		console.error("Error fetching combos:", error);
		combos.value = [];
	}
}

async function fetchProviderNodes() {
	try {
		const res = await fetch("/api/provider-nodes");
		if (!res.ok) throw new Error(`Failed to fetch provider nodes: ${res.status}`);
		const data = await res.json();
		providerNodes.value = data.nodes || [];
	} catch (error) {
		console.error("Error fetching provider nodes:", error);
		providerNodes.value = [];
	}
}

async function fetchCustomModels() {
	try {
		const res = await fetch("/api/models/custom");
		if (!res.ok) throw new Error(`Failed to fetch custom models: ${res.status}`);
		const data = await res.json();
		customModels.value = data.models || [];
	} catch (error) {
		console.error("Error fetching custom models:", error);
		customModels.value = [];
	}
}

async function fetchDisabledModels() {
	try {
		const res = await fetch("/api/models/disabled");
		if (!res.ok) throw new Error(`Failed to fetch disabled models: ${res.status}`);
		const data = await res.json();
		disabledModels.value = data.disabled || {};
	} catch (error) {
		console.error("Error fetching disabled models:", error);
		disabledModels.value = {};
	}
}

watch(
	() => props.isOpen,
	(open) => {
		if (!open) return;
		fetchCombos();
		fetchProviderNodes();
		fetchCustomModels();
		fetchDisabledModels();
	},
	{ immediate: true },
);

const allProviders = computed(() => ({
	...OAUTH_PROVIDERS,
	...FREE_PROVIDERS,
	...FREE_TIER_PROVIDERS,
	...APIKEY_PROVIDERS,
}));

// Filter activeProviders by serviceKinds when kindFilter set (e.g. "webSearch", "webFetch")
const filteredActiveProviders = computed(() => {
	if (!props.kindFilter) return props.activeProviders;
	return props.activeProviders.filter((p) => {
		const info = AI_PROVIDERS[p.provider ?? ""];
		const kinds = info?.serviceKinds || ["llm"];
		return kinds.includes(props.kindFilter);
	});
});

// Group models by provider with priority order
const groupedModels = computed<Record<string, any>>(() => {
	const groups: Record<string, any> = {};

	// Kinds where the provider IS the model (no per-model selection needed)
	const PROVIDER_AS_MODEL_KINDS = new Set(["webSearch", "webFetch"]);
	// Kinds that map directly to model.type field
	const TYPED_KINDS = new Set(["embedding"]);
	// For these kinds, providers without hardcoded models can still be picked (provider-as-model fallback)
	const ALLOW_PROVIDER_FALLBACK_KINDS = new Set(["webFetch"]);

	// Filter a models[] array by kindFilter (keep only matching kind)
	const filterByKind = (models: Array<Record<string, any>>) => {
		// No kindFilter means the LLM selector. Keep custom models visible because
		// user-added models may have typed capabilities while still being valid
		// chat/combo targets.
		if (!props.kindFilter)
			return models.filter(
				(m) => m.isPlaceholder || m.isCustom || !getModelKind(m) || getModelKind(m) === "llm",
			);
		if (!TYPED_KINDS.has(props.kindFilter)) return models;
		return models.filter((m) => m.isPlaceholder || getModelKind(m) === props.kindFilter);
	};

	// Get all active provider IDs from connections (filtered by kindFilter if set)
	const activeConnectionIds = filteredActiveProviders.value.map((p) => p.provider);

	// No-auth providers: filter by kindFilter as well
	const noAuthIds = props.kindFilter
		? NO_AUTH_PROVIDER_IDS.filter((id) => (AI_PROVIDERS[id]?.serviceKinds || ["llm"]).includes(props.kindFilter as string))
		: NO_AUTH_PROVIDER_IDS;

	// Only show connected providers (including both standard and custom)
	const providerIdsToShow = new Set([...activeConnectionIds, ...noAuthIds]);

	// Sort by PROVIDER_ORDER
	const sortedProviderIds = [...providerIdsToShow].sort((a, b) => {
		const indexA = PROVIDER_ORDER.indexOf(a as string);
		const indexB = PROVIDER_ORDER.indexOf(b as string);
		return (indexA === -1 ? 999 : indexA) - (indexB === -1 ? 999 : indexB);
	});

	for (const providerId of sortedProviderIds) {
		if (!providerId) continue;
		const alias = getProviderAlias(providerId);
		const providerInfo = allProviders.value[providerId] || { name: providerId, color: "#666" };
		const isCustomProvider = isOpenAICompatibleProvider(providerId) || isAnthropicCompatibleProvider(providerId);

		// For provider-as-model kinds (webSearch/webFetch): emit a single entry where value === providerId
		if (props.kindFilter && PROVIDER_AS_MODEL_KINDS.has(props.kindFilter)) {
			groups[providerId] = {
				name: providerInfo.name,
				alias,
				color: providerInfo.color,
				models: [{ id: providerId, name: providerInfo.name, value: providerId }],
			};
			continue;
		}

		if (providerInfo.passthroughModels) {
			const aliasModels = Object.entries(props.modelAliases)
				.filter(([, fullModel]) => fullModel.startsWith(`${alias}/`))
				.map(([aliasName, fullModel]) => ({
					id: fullModel.replace(`${alias}/`, ""),
					name: aliasName,
					value: fullModel,
				}));
			const customRegisteredModels = customModels.value
				.filter((m) => m.providerAlias === alias)
				.map((m) => ({
					id: m.id,
					name: m.name || m.id,
					value: `${alias}/${m.id}`,
					kind: getModelKind(m),
					isCustom: true,
				}));

			// For typed kinds, only include hardcoded typed models (aliases are typically LLM-only)
			let combined = aliasModels;
			if (props.kindFilter && TYPED_KINDS.has(props.kindFilter)) {
				const registeredTyped = customRegisteredModels.filter((m) => getModelKind(m) === props.kindFilter);
				combined = [
					...registeredTyped,
					...getModelsByProviderId(providerId)
						.filter((m) => getModelKind(m) === props.kindFilter)
						.map((m) => ({ id: m.id, name: m.name, value: `${alias}/${m.id}`, kind: getModelKind(m) }))
						.filter((m) => !registeredTyped.some((registered) => registered.value === m.value)),
				];
				// Fallback: provider-as-model when no hardcoded models match (webFetch only)
				if (combined.length === 0 && ALLOW_PROVIDER_FALLBACK_KINDS.has(props.kindFilter)) {
					const supports = (providerInfo.serviceKinds || ["llm"]).includes(props.kindFilter);
					if (supports) combined = [{ id: providerId, name: providerInfo.name, value: alias }];
				}
			} else {
				// LLM/null kind: merge hardcoded models with user-added models
				const registeredLlms = customRegisteredModels.filter((m) => !getModelKind(m) || getModelKind(m) === "llm");
				const seen = new Set([...aliasModels, ...registeredLlms].map((m) => m.value));
				const hardcoded = getModelsByProviderId(providerId)
					.filter((m) => !getModelKind(m) || getModelKind(m) === "llm")
					.map((m) => ({ id: m.id, name: m.name, value: `${alias}/${m.id}`, kind: getModelKind(m) }))
					.filter((m) => !seen.has(m.value));
				combined = [
					...registeredLlms,
					...aliasModels.filter((m) => !registeredLlms.some((registered) => registered.value === m.value)),
					...hardcoded,
				];
			}

			if (combined.length > 0) {
				// Check for custom name from providerNodes (for compatible providers)
				const matchedNode = providerNodes.value.find((node) => node.id === providerId);
				const displayName = matchedNode?.name || providerInfo.name;

				groups[providerId] = {
					name: displayName,
					alias,
					color: providerInfo.color,
					models: combined,
				};
			}
		} else if (isCustomProvider) {
			// Custom (openai/anthropic-compatible) providers are LLM-only — skip for typed media kinds
			if (props.kindFilter && TYPED_KINDS.has(props.kindFilter)) continue;
			// Find connection object to get prefix synchronously without waiting for providerNodes fetch
			const connection = props.activeProviders.find((p) => p.provider === providerId);
			const matchedNode = providerNodes.value.find((node) => node.id === providerId);
			const displayName = matchedNode?.name || connection?.name || providerInfo.name;
			const nodePrefix = connection?.providerSpecificData?.prefix || matchedNode?.prefix || providerId;

			// Aliases are stored using the raw providerId as key, so filter by
			// providerId, not by the display prefix.
			const nodeModels = Object.entries(props.modelAliases)
				.filter(([, fullModel]) => fullModel.startsWith(`${providerId}/`))
				.map(([aliasName, fullModel]) => ({
					id: fullModel.replace(`${providerId}/`, ""),
					name: aliasName,
					value: `${nodePrefix}/${fullModel.replace(`${providerId}/`, "")}`,
				}));

			// Merge custom models registered via /api/models/custom for this provider
			const registeredCustom = customModels.value
				.filter((m) => m.providerAlias === providerId)
				.map((m) => ({
					id: m.id,
					name: m.name || m.id,
					value: `${nodePrefix}/${m.id}`,
					isCustom: true,
				}));
			const seen = new Set(nodeModels.map((m) => m.value));
			const mergedModels = [...nodeModels, ...registeredCustom.filter((m) => !seen.has(m.value))];

			// Always show compatible providers that are connected, even with no aliases.
			const modelsToShow =
				mergedModels.length > 0
					? mergedModels
					: [
							{
								id: `__placeholder__${providerId}`,
								name: `${nodePrefix}/model-id`,
								value: `${nodePrefix}/model-id`,
								isPlaceholder: true,
							},
						];

			groups[providerId] = {
				name: displayName,
				alias: nodePrefix,
				color: providerInfo.color,
				models: modelsToShow,
				isCustom: true,
				hasModels: mergedModels.length > 0,
			};
		} else {
			const hardcodedModels = getModelsByProviderId(providerId);
			const hardcodedIds = new Set(hardcodedModels.map((m) => m.id));

			// Custom models: if no hardcoded models (e.g. openrouter), show all aliases for
			// this provider. Otherwise only show aliases where aliasName === modelId.
			const hasHardcoded = hardcodedModels.length > 0;
			const customAliasModels = Object.entries(props.modelAliases)
				.filter(
					([aliasName, fullModel]) =>
						fullModel.startsWith(`${alias}/`) &&
						(hasHardcoded ? aliasName === fullModel.replace(`${alias}/`, "") : true) &&
						!hardcodedIds.has(fullModel.replace(`${alias}/`, "")),
				)
				.map(([aliasName, fullModel]) => {
					const modelId = fullModel.replace(`${alias}/`, "");
					return { id: modelId, name: aliasName, value: fullModel, isCustom: true };
				});

			// Custom models registered via /api/models/custom (provider "Add Model" button)
			const customAliasIds = new Set(customAliasModels.map((m) => m.id));
			const customRegisteredModels = customModels.value
				.filter((m) => m.providerAlias === alias && !hardcodedIds.has(m.id) && !customAliasIds.has(m.id))
				.map((m) => ({ id: m.id, name: m.name || m.id, value: `${alias}/${m.id}`, isCustom: true }));

			const merged = [
				...hardcodedModels.map((m) => ({ id: m.id, name: m.name, value: `${alias}/${m.id}`, kind: getModelKind(m) })),
				...customAliasModels,
				...customRegisteredModels,
			];
			// Dedupe by value (alias may equal hardcoded id)
			const seen = new Set();
			let allModels = filterByKind(
				merged.filter((m) => {
					if (seen.has(m.value)) return false;
					seen.add(m.value);
					return true;
				}),
			);

			// Provider-as-model fallback: providers that support the kind but have no
			// hardcoded models can still be picked (value = providerAlias).
			if (allModels.length === 0 && props.kindFilter && ALLOW_PROVIDER_FALLBACK_KINDS.has(props.kindFilter)) {
				const supports = (providerInfo.serviceKinds || ["llm"]).includes(props.kindFilter);
				if (supports) {
					allModels = [{ id: providerId, name: providerInfo.name, value: alias }];
				}
			}

			if (allModels.length > 0) {
				groups[providerId] = {
					name: providerInfo.name,
					alias,
					color: providerInfo.color,
					models: allModels,
				};
			}
		}
	}

	// Filter out disabled models per provider (disabled keyed by storage alias OR providerId)
	for (const [providerId, group] of Object.entries(groups)) {
		const aliasKey = getProviderAlias(providerId);
		const disabledIds = new Set([
			...(disabledModels.value[aliasKey] || []),
			...(disabledModels.value[providerId] || []),
		]);
		if (disabledIds.size === 0) continue;
		group.models = group.models.filter((m: any) => !disabledIds.has(m.id));
		if (group.models.length === 0) delete groups[providerId];
	}

	return groups;
});

// Filter combos by search query (and hide combos when kindFilter is set — combos are LLM-only)
const filteredCombos = computed(() => {
	if (props.kindFilter || props.capFilter) return [];
	if (!searchQuery.value.trim()) return combos.value;
	const query = searchQuery.value.toLowerCase();
	return combos.value.filter((c) => c.name.toLowerCase().includes(query));
});

// Sort models alphabetically, with added models floated to top
function sortModels(models: Array<Record<string, any>>) {
	const added = models
		.filter((m) => props.addedModelValues.includes(m.value))
		.sort((a, b) => a.name.localeCompare(b.name));
	const rest = models
		.filter((m) => !props.addedModelValues.includes(m.value))
		.sort((a, b) => a.name.localeCompare(b.name));
	return [...added, ...rest];
}

// Filter models by search query
const filteredGroups = computed(() => {
	const query = searchQuery.value.trim().toLowerCase();

	const filtered: Record<string, any> = {};
	for (const [providerId, group] of Object.entries(groupedModels.value)) {
		let models = group.models;
		// Filter by input-modality capability (vision/pdf/audioInput/videoInput).
		if (props.capFilter) {
			models = models.filter(
				(m: any) => (getCaps(m.value) as Record<string, any> | null)?.[props.capFilter as string] === true,
			);
			if (models.length === 0) continue;
		}
		if (query) {
			const providerNameMatches = group.name.toLowerCase().includes(query);
			models = models.filter(
				(m: any) => m.name.toLowerCase().includes(query) || m.id.toLowerCase().includes(query),
			);
			if (models.length === 0 && !providerNameMatches) continue;
		}
		filtered[providerId] = { ...group, models: sortModels(models) };
	}

	return filtered;
});

function handleSelect(model: any) {
	const value = model?.value || model?.name || model;
	const isAdded = props.addedModelValues.includes(value);

	if (isAdded) {
		emit("deselect", model);
	} else {
		emit("select", model);
	}

	if (props.closeOnSelect) {
		emit("close");
		searchQuery.value = "";
	}
}

function handleClose() {
	emit("close");
	searchQuery.value = "";
}
</script>

<template>
  <Modal :is-open="props.isOpen" :title="props.title" size="md" class-name="p-4!" @close="handleClose">
    <!-- Info bar -->
    <div class="flex items-center gap-2 mb-3 px-2.5 py-2 bg-primary/8 border border-primary/20 rounded-lg text-xs text-text-muted">
      <span class="material-symbols-outlined text-primary shrink-0" style="font-size: 14px">info</span>
      <span>Click to add, click again to remove. Changes are saved automatically.</span>
    </div>

    <!-- Search - compact -->
    <div class="mb-3">
      <div class="relative">
        <span class="material-symbols-outlined absolute left-2.5 top-1/2 -translate-y-1/2 text-text-muted text-[16px]">
          search
        </span>
        <input
          v-model="searchQuery"
          type="text"
          placeholder="Search..."
          class="w-full pl-8 pr-3 py-1.5 bg-surface border border-border rounded text-xs focus:outline-none focus:ring-1 focus:ring-primary/50"
        />
      </div>
    </div>

    <!-- Models grouped by provider - compact -->
    <div class="max-h-100 overflow-y-auto space-y-3">
      <!-- Combos section - always first -->
      <div v-if="filteredCombos.length > 0">
        <div class="flex items-center gap-1.5 mb-1.5 sticky top-0 bg-surface py-0.5">
          <span class="material-symbols-outlined text-primary text-[14px]">layers</span>
          <span class="text-xs font-medium text-primary">Combos</span>
          <span class="text-[10px] text-text-muted">({{ filteredCombos.length }})</span>
        </div>
        <div class="flex flex-wrap gap-1.5">
          <button
            v-for="combo in filteredCombos"
            :key="combo.id"
            type="button"
            :class="`
              px-2 py-1 rounded-xl text-xs font-medium transition-all border hover:cursor-pointer flex items-center gap-1
              ${
								props.selectedModel === combo.name
									? 'bg-primary text-white border-primary'
									: props.addedModelValues.includes(combo.name)
										? 'bg-primary border-primary text-white hover:bg-primary-hover'
										: 'bg-surface border-border text-text-main hover:border-primary/50 hover:bg-primary/5'
							}
            `"
            @click="handleSelect({ id: combo.name, name: combo.name, value: combo.name })"
          >
            <span
              v-if="props.addedModelValues.includes(combo.name)"
              class="material-symbols-outlined leading-none"
              style="font-size: 10px"
              >check</span
            >
            {{ combo.name }}
          </button>
        </div>
      </div>

      <!-- Provider models -->
      <div v-for="(group, providerId) in filteredGroups" :key="providerId">
        <!-- Provider header -->
        <div class="flex items-center gap-1.5 mb-1.5 sticky top-0 bg-surface py-0.5">
          <ProviderIcon
            :src="`/providers/${providerId}.png`"
            :alt="group.name"
            :size="14"
            :fallback-text="(group.name || providerId).slice(0, 2).toUpperCase()"
            :fallback-color="group.color"
          />
          <span class="text-xs font-medium text-primary">{{ group.name }}</span>
          <span class="text-[10px] text-text-muted">({{ group.models.length }})</span>
        </div>

        <div class="flex flex-wrap gap-1.5">
          <button
            v-for="model in group.models"
            :key="model.value"
            type="button"
            :title="model.isPlaceholder ? 'Select to pre-fill, then edit model ID in the input' : undefined"
            :class="`
              px-2 py-1 rounded-xl text-xs font-medium transition-all border hover:cursor-pointer
              ${
								model.isPlaceholder
									? 'border-dashed border-border text-text-muted hover:border-primary/50 hover:text-primary bg-surface italic'
									: props.selectedModel === model.value
										? 'bg-primary text-white border-primary'
										: props.addedModelValues.includes(model.value)
											? 'bg-primary border-primary text-white hover:bg-primary-hover'
											: 'bg-surface border-border text-text-main hover:border-primary/50 hover:bg-primary/5'
							}
            `"
            @click="handleSelect(model)"
          >
            <span class="flex items-center gap-1">
              <span
                v-if="props.addedModelValues.includes(model.value) && !model.isPlaceholder"
                class="material-symbols-outlined leading-none"
                style="font-size: 10px"
                >check</span
              >
              <template v-if="model.isPlaceholder">
                <span class="material-symbols-outlined text-[11px]">edit</span>
                {{ model.name }}
              </template>
              <template v-else-if="model.isCustom">
                {{ model.name }}
                <span class="text-[9px] opacity-60 font-normal">custom</span>
                <CapacityBadges :caps="getCaps(model.value)" />
              </template>
              <template v-else>
                {{ model.name }}
                <CapacityBadges :caps="getCaps(model.value)" />
              </template>
            </span>
          </button>
        </div>
      </div>

      <div v-if="Object.keys(filteredGroups).length === 0 && filteredCombos.length === 0" class="text-center py-4 text-text-muted">
        <span class="material-symbols-outlined text-2xl mb-1 block">search_off</span>
        <p class="text-xs">No models found</p>
      </div>
    </div>
  </Modal>
</template>
