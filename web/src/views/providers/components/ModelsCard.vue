<script setup lang="ts">
import { computed, onMounted, ref } from "vue";

import Card from "@/components/ui/UiCard.vue";
import { getModelKind, useModels } from "@/constants/models";
import { useProviders } from "@/constants/providers";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";
import AddCustomModelModal from "./AddCustomModelModal.vue";
import ModelRow from "./ModelRow.vue";

// Self-contained card: shows models for a provider, filtered by optional `kindFilter`.
// kindFilter: if provided, only shows models with matching type/kinds field.
const props = defineProps<{
	providerId: string;
	kindFilter?: string;
	providerAliasOverride?: string;
}>();

const { copied, copy } = useCopyToClipboard();
const { getModelsByProviderId } = useModels();
const { getProviderAlias } = useProviders();

const modelAliases = ref<Record<string, string>>({});
const customModels = ref<Array<Record<string, any>>>([]);
const modelTestResults = ref<Record<string, "ok" | "error">>({});
const testingModelId = ref<string | null>(null);
const testError = ref("");
const showAddCustomModel = ref(false);

const providerAlias = computed(
	() => props.providerAliasOverride || getProviderAlias(props.providerId),
);
const effectiveType = computed(() => props.kindFilter || "llm");

async function fetchData() {
	try {
		const [aliasRes, customRes] = await Promise.all([
			fetch("/api/models/alias"),
			fetch("/api/models/custom", { cache: "no-store" }),
		]);
		const aliasData = await aliasRes.json();
		const customData = await customRes.json();
		if (aliasRes.ok) modelAliases.value = aliasData.aliases || {};
		if (customRes.ok) customModels.value = customData.models || [];
	} catch (e) {
		console.log("ModelsCard fetch error:", e);
	}
}

onMounted(fetchData);

async function handleDeleteAlias(alias: string) {
	try {
		const res = await fetch(`/api/models/alias?alias=${encodeURIComponent(alias)}`, {
			method: "DELETE",
		});
		if (res.ok) await fetchData();
	} catch (e) {
		console.log("delete alias error:", e);
	}
}

async function handleAddCustomModel(modelId: string) {
	try {
		const res = await fetch("/api/models/custom", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				providerAlias: providerAlias.value,
				id: modelId,
				type: effectiveType.value,
			}),
		});
		if (res.ok) {
			await fetchData();
			window.dispatchEvent(new CustomEvent("customModelChanged"));
		}
	} catch (e) {
		console.log("add custom model error:", e);
	}
}

async function handleDeleteCustomModel(modelId: string) {
	try {
		const params = new URLSearchParams({
			providerAlias: providerAlias.value,
			id: modelId,
			type: effectiveType.value,
		});
		const res = await fetch(`/api/models/custom?${params}`, { method: "DELETE" });
		if (res.ok) {
			await fetchData();
			window.dispatchEvent(new CustomEvent("customModelChanged"));
		}
	} catch (e) {
		console.log("delete custom model error:", e);
	}
}

async function handleTestModel(modelId: string) {
	if (testingModelId.value) return;
	testingModelId.value = modelId;
	try {
		const res = await fetch("/api/models/test", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				model: `${providerAlias.value}/${modelId}`,
				kind: props.kindFilter,
			}),
		});
		const data = await res.json();
		modelTestResults.value = {
			...modelTestResults.value,
			[modelId]: data.ok ? "ok" : "error",
		};
		testError.value = data.ok ? "" : data.error || "Model not reachable";
	} catch {
		modelTestResults.value = { ...modelTestResults.value, [modelId]: "error" };
		testError.value = "Network error";
	} finally {
		testingModelId.value = null;
	}
}

// Built-in models — filter by kindFilter if provided
const allBuiltIn = computed<Array<Record<string, any>>>(() =>
	getModelsByProviderId(props.providerId),
);
const builtInModels = computed(() =>
	props.kindFilter
		? allBuiltIn.value.filter((m) => {
				if (m.kinds) return m.kinds.includes(props.kindFilter);
				return getModelKind(m, "llm") === props.kindFilter;
			})
		: allBuiltIn.value,
);

// Custom models for this provider + kind, dedupe vs built-in
const myCustomModels = computed(() =>
	customModels.value.filter(
		(m) =>
			m.providerAlias === providerAlias.value &&
			getModelKind(m, "llm") === effectiveType.value &&
			!builtInModels.value.some((b) => b.id === m.id),
	),
);

const displayModels = computed(() => builtInModels.value);

function existingAliasFor(fullModel: string): string | undefined {
	return Object.entries(modelAliases.value).find(([, m]) => m === fullModel)?.[0];
}

async function onAddCustomModelSave(modelId: string) {
	await handleAddCustomModel(modelId);
	showAddCustomModel.value = false;
}
</script>

<template>
  <Card>
    <div class="flex items-center justify-between mb-4">
      <h2 class="text-lg font-semibold">Models{{ props.kindFilter ? ` — ${props.kindFilter.toUpperCase()}` : "" }}</h2>
    </div>
    <p v-if="testError" class="text-xs text-red-500 mb-3 wrap-break-word">{{ testError }}</p>

    <div class="flex flex-wrap gap-3">
      <ModelRow
        v-for="model in displayModels"
        :key="model.id"
        :model="model"
        :full-model="`${providerAlias}/${model.id}`"
        :copied="copied"
        :test-status="modelTestResults[model.id]"
        :is-testing="testingModelId === model.id"
        :is-free="model.isFree"
        @copy="copy"
        @delete-alias="handleDeleteAlias(existingAliasFor(`${providerAlias}/${model.id}`) as string)"
        @test="handleTestModel(model.id)"
      />

      <ModelRow
        v-for="model in myCustomModels"
        :key="`${model.id}-${model.type}`"
        :model="{ id: model.id, name: model.name }"
        :full-model="`${providerAlias}/${model.id}`"
        :copied="copied"
        :test-status="modelTestResults[model.id]"
        :is-testing="testingModelId === model.id"
        is-custom
        @copy="copy"
        @delete-alias="handleDeleteCustomModel(model.id)"
        @test="handleTestModel(model.id)"
      />

      <button
        type="button"
        class="flex items-center gap-1.5 px-3 py-2 rounded-lg border border-dashed border-black/15 dark:border-white/15 text-xs text-text-muted hover:text-primary hover:border-primary/40 transition-colors"
        @click="showAddCustomModel = true"
      >
        <span class="material-symbols-outlined text-sm">add</span>
        Add Model
      </button>
    </div>
  </Card>

  <AddCustomModelModal
    :is-open="showAddCustomModel"
    @save="onAddCustomModelSave"
    @close="showAddCustomModel = false"
  />
</template>
