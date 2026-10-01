<script setup lang="ts">
import { computed, ref } from "vue";

import Button from "@/components/ui/UiButton.vue";
import { getProviderCustomModelRows } from "@/utils/providerCustomModels";

const props = withDefaults(
	defineProps<{
		providerStorageAlias: string;
		providerDisplayAlias: string;
		modelAliases: Record<string, string>;
		customModels?: Array<Record<string, any>>;
		copied?: string | null;
		connections: Array<Record<string, any>>;
		isAnthropic?: boolean;
	}>(),
	{ customModels: () => [], copied: null, isAnthropic: false },
);

const emit = defineEmits<{
	copy: [text: string, id: string];
	deleteAlias: [alias: string];
	addCustomModel: [modelId: string];
	deleteCustomModel: [modelId: string];
}>();

const newModel = ref("");
const adding = ref(false);
const importing = ref(false);
const testingModelId = ref<string | null>(null);
const modelTestResults = ref<Record<string, "ok" | "error">>({});

const allModels = computed(() =>
	getProviderCustomModelRows({
		customModels: props.customModels,
		modelAliases: props.modelAliases,
		providerAlias: props.providerStorageAlias,
		type: "llm",
	}),
);

const canImport = computed(() => props.connections.some((conn) => conn.isActive !== false));

async function handleTestModel(modelId: string) {
	if (testingModelId.value) return;
	testingModelId.value = modelId;
	try {
		const res = await fetch("/api/models/test", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ model: `${props.providerStorageAlias}/${modelId}` }),
		});
		const data = await res.json();
		modelTestResults.value = { ...modelTestResults.value, [modelId]: data.ok ? "ok" : "error" };
	} catch {
		modelTestResults.value = { ...modelTestResults.value, [modelId]: "error" };
	} finally {
		testingModelId.value = null;
	}
}

async function handleAdd() {
	if (!newModel.value.trim() || adding.value) return;
	const modelId = newModel.value.trim();
	if (allModels.value.some((model) => model.id === modelId)) {
		alert("Model already exists for this provider.");
		return;
	}

	adding.value = true;
	try {
		emit("addCustomModel", modelId);
		newModel.value = "";
	} catch (error) {
		console.log("Error adding model:", error);
	} finally {
		adding.value = false;
	}
}

async function handleImport() {
	if (importing.value) return;
	const activeConnection = props.connections.find((conn) => conn.isActive !== false);
	if (!activeConnection) return;

	importing.value = true;
	try {
		const res = await fetch(`/api/providers/${activeConnection.id}/models`);
		const data = await res.json();
		if (!res.ok) {
			alert(data.error || "Failed to import models");
			return;
		}
		const models = data.models || [];
		if (models.length === 0) {
			alert("No models returned from /models.");
			return;
		}
		let importedCount = 0;
		for (const model of models) {
			const modelId = model.id || model.name || model.model;
			if (!modelId) continue;
			if (allModels.value.some((entry) => entry.id === modelId)) continue;
			emit("addCustomModel", modelId);
			importedCount += 1;
		}
		if (importedCount === 0) {
			alert("No new models were added.");
		}
	} catch (error) {
		console.log("Error importing models:", error);
	} finally {
		importing.value = false;
	}
}

function borderColor(testStatus?: "ok" | "error") {
	return testStatus === "ok"
		? "border-green-500/40"
		: testStatus === "error"
			? "border-red-500/40"
			: "border-border";
}

function iconColor(testStatus?: "ok" | "error") {
	return testStatus === "ok" ? "#22c55e" : testStatus === "error" ? "#ef4444" : undefined;
}

function iconName(testStatus?: "ok" | "error") {
	return testStatus === "ok" ? "check_circle" : testStatus === "error" ? "cancel" : "smart_toy";
}
</script>

<template>
  <div class="flex flex-col gap-4">
    <p class="text-sm text-text-muted">
      Add {{ props.isAnthropic ? "Anthropic" : "OpenAI" }}-compatible models manually or import them from the /models endpoint.
    </p>

    <div class="flex items-end gap-2 flex-wrap">
      <div class="flex-1 min-w-60">
        <label for="new-compatible-model-input" class="text-xs text-text-muted mb-1 block">Model ID</label>
        <input
          id="new-compatible-model-input"
          v-model="newModel"
          type="text"
          :placeholder="props.isAnthropic ? 'claude-3-opus-20240229' : 'gpt-4o'"
          class="w-full px-3 py-2 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
          @keydown="(e: KeyboardEvent) => e.key === 'Enter' && handleAdd()"
        />
      </div>
      <Button size="sm" icon="add" :disabled="!newModel.trim() || adding" @click="handleAdd">
        {{ adding ? "Adding..." : "Add" }}
      </Button>
      <Button size="sm" variant="secondary" icon="download" :disabled="!canImport || importing" @click="handleImport">
        {{ importing ? "Importing..." : "Import from /models" }}
      </Button>
    </div>

    <p v-if="!canImport" class="text-xs text-text-muted">
      Add a connection to enable importing models.
    </p>

    <div v-if="allModels.length > 0" class="flex flex-col gap-3">
      <div
        v-for="model in allModels"
        :key="`${model.source}-${props.providerStorageAlias}/${model.id}`"
        :class="`flex items-center gap-3 p-3 rounded-lg border ${borderColor(modelTestResults[model.id])} hover:bg-sidebar/50`"
      >
        <span
          class="material-symbols-outlined text-base text-text-muted"
          :style="iconColor(modelTestResults[model.id]) ? { color: iconColor(modelTestResults[model.id]) } : undefined"
        >{{ iconName(modelTestResults[model.id]) }}</span>
        <div class="flex-1 min-w-0">
          <p class="text-sm font-medium truncate">{{ model.id }}</p>
          <div class="flex items-center gap-1 mt-1">
            <code class="text-xs text-text-muted font-mono bg-sidebar px-1.5 py-0.5 rounded">{{ props.providerDisplayAlias }}/{{ model.id }}</code>
            <div class="relative group/btn">
              <button
                type="button"
                class="p-0.5 hover:bg-sidebar rounded text-text-muted hover:text-primary"
                @click="emit('copy', `${props.providerDisplayAlias}/${model.id}`, `model-${model.id}`)"
              >
                <span class="material-symbols-outlined text-sm">{{ props.copied === `model-${model.id}` ? "check" : "content_copy" }}</span>
              </button>
              <span class="pointer-events-none absolute top-5 left-1/2 -translate-x-1/2 text-[10px] text-text-muted whitespace-nowrap opacity-0 group-hover/btn:opacity-100 transition-opacity">
                {{ props.copied === `model-${model.id}` ? "Copied!" : "Copy" }}
              </span>
            </div>
            <div v-if="props.connections.length > 0" class="relative group/btn">
              <button
                type="button"
                :disabled="testingModelId === model.id"
                class="p-0.5 hover:bg-sidebar rounded text-text-muted hover:text-primary transition-colors"
                @click="handleTestModel(model.id)"
              >
                <span
                  class="material-symbols-outlined text-sm"
                  :style="testingModelId === model.id ? { animation: 'spin 1s linear infinite' } : undefined"
                >{{ testingModelId === model.id ? "progress_activity" : "science" }}</span>
              </button>
              <span class="pointer-events-none absolute top-5 left-1/2 -translate-x-1/2 text-[10px] text-text-muted whitespace-nowrap opacity-0 group-hover/btn:opacity-100 transition-opacity">
                {{ testingModelId === model.id ? "Testing..." : "Test" }}
              </span>
            </div>
          </div>
        </div>
        <button
          type="button"
          class="p-1 hover:bg-red-50 rounded text-red-500"
          title="Remove model"
          @click="model.source === 'custom' ? emit('deleteCustomModel', model.id) : emit('deleteAlias', model.alias)"
        >
          <span class="material-symbols-outlined text-sm">delete</span>
        </button>
      </div>
    </div>
  </div>
</template>
