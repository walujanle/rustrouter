<script setup lang="ts">
import { ref, useId, watch } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Modal from "@/components/ui/UiModal.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { CAPACITY_META } from "@/constants/models";

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		providerAlias?: string;
		providerDisplayAlias?: string;
	}>(),
	{ providerAlias: "", providerDisplayAlias: "" },
);

const emit = defineEmits<{
	save: [modelId: string, caps: Record<string, any>];
	close: [];
}>();

const defaultCaps = () =>
	Object.fromEntries(Object.keys(CAPACITY_META).map((key) => [key, false]));

const uid = useId();

const modelId = ref("");
const caps = ref<Record<string, any>>(defaultCaps());
const testStatus = ref<"testing" | "ok" | "error" | null>(null);
const testError = ref("");
const saving = ref(false);

watch(
	() => props.isOpen,
	(isOpen) => {
		if (isOpen) {
			modelId.value = "";
			caps.value = defaultCaps();
			testStatus.value = null;
			testError.value = "";
		}
	},
);

// Strip provider's own alias prefix (e.g. "cc/model" -> "model" for cc provider)
function stripAlias(id: string): string {
	const prefix = `${props.providerAlias}/`;
	return id.startsWith(prefix) ? id.slice(prefix.length) : id;
}

async function handleTest() {
	const cleanId = stripAlias(modelId.value.trim());
	if (!cleanId) return;
	testStatus.value = "testing";
	testError.value = "";
	try {
		const res = await fetch("/api/models/test", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				model: props.providerAlias ? `${props.providerAlias}/${cleanId}` : cleanId,
			}),
		});
		const data = await res.json();
		testStatus.value = data.ok ? "ok" : "error";
		testError.value = data.error || "";
	} catch (err) {
		testStatus.value = "error";
		testError.value = (err as Error).message;
	}
}

async function handleSave() {
	const cleanId = stripAlias(modelId.value.trim());
	if (!cleanId || saving.value) return;
	saving.value = true;
	try {
		emit("save", cleanId, caps.value);
	} finally {
		saving.value = false;
	}
}

function handleKeyDown(e: KeyboardEvent) {
	if (e.key === "Enter") handleTest();
}
</script>

<template>
  <Modal :is-open="props.isOpen" title="Add Custom Model" @close="emit('close')">
    <div class="flex flex-col gap-4">
      <div>
        <label :for="`custom-model-id-${uid}`" class="text-sm font-medium mb-1.5 block">Model ID</label>
        <div class="flex gap-2">
          <input
            :id="`custom-model-id-${uid}`"
            type="text"
            :value="modelId"
            placeholder="e.g. claude-opus-4-5"
            class="flex-1 px-3 py-2 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
            @input="(e: Event) => { modelId = (e.target as HTMLInputElement).value; testStatus = null; testError = ''; }"
            @keydown="handleKeyDown"
          />
          <Button
            variant="secondary"
            icon="science"
            :loading="testStatus === 'testing'"
            :disabled="!modelId.trim() || testStatus === 'testing'"
            @click="handleTest"
          >
            {{ testStatus === "testing" ? "Testing..." : "Test" }}
          </Button>
        </div>
        <p class="text-xs text-text-muted mt-1">
          Sent to provider as: <code class="font-mono bg-sidebar px-1 rounded">{{ stripAlias(modelId.trim()) || "model-id" }}</code>
        </p>
      </div>

      <fieldset class="border-0 p-0 m-0 min-w-0">
        <legend class="text-sm font-medium mb-1.5">Capabilities</legend>
        <div class="flex flex-wrap gap-4">
          <Toggle
            v-for="(meta, key) in CAPACITY_META"
            :key="key"
            :model-value="!!caps[key]"
            :label="meta.label"
            :description="meta.desc"
            size="sm"
            @update:model-value="(v: boolean) => { caps = { ...caps, [key]: v }; }"
          />
        </div>
      </fieldset>

      <!-- Test result -->
      <div v-if="testStatus === 'ok'" class="flex items-center gap-2 text-sm text-green-600">
        <span class="material-symbols-outlined text-base">check_circle</span>
        Model is reachable
      </div>
      <div v-if="testStatus === 'error'" class="flex items-start gap-2 text-sm text-red-500">
        <span class="material-symbols-outlined text-base shrink-0">cancel</span>
        <span>{{ testError || "Model not reachable" }}</span>
      </div>

      <div class="flex gap-2 pt-1">
        <Button variant="ghost" full-width size="sm" @click="emit('close')">Cancel</Button>
        <Button
          full-width
          size="sm"
          :disabled="!modelId.trim() || saving"
          @click="handleSave"
        >
          {{ saving ? "Adding..." : "Add Model" }}
        </Button>
      </div>
    </div>
  </Modal>
</template>
