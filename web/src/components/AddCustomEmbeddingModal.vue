<script setup lang="ts">
import { computed, ref, watch } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";

// Dual-mode add/edit for custom-embedding provider nodes, plus a "Check" that
// validates a key + model id against `/api/provider-nodes/validate`.

const DEFAULT_BASE_URL = "https://api.openai.com/v1";

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		node?: Record<string, any> | null;
	}>(),
	{ node: null },
);

const emit = defineEmits<{ close: []; created: [node: Record<string, any>]; saved: [node: Record<string, any>] }>();

const isEdit = computed(() => !!props.node);

const formData = ref({ name: "", prefix: "", baseUrl: DEFAULT_BASE_URL });
const submitting = ref(false);
const checkKey = ref("");
const checkModelId = ref("");
const validating = ref(false);
const validationResult = ref<Record<string, any> | null>(null);

watch(
	() => [props.isOpen, props.node] as const,
	() => {
		if (!props.isOpen) return;
		validationResult.value = null;
		checkKey.value = "";
		checkModelId.value = "";
		if (props.node) {
			formData.value = {
				name: props.node.name || "",
				prefix: props.node.prefix || "",
				baseUrl: props.node.baseUrl || DEFAULT_BASE_URL,
			};
		} else {
			formData.value = { name: "", prefix: "", baseUrl: DEFAULT_BASE_URL };
		}
	},
	{ immediate: true },
);

const canSubmit = computed(
	() =>
		!!formData.value.name.trim() &&
		!!formData.value.prefix.trim() &&
		!!formData.value.baseUrl.trim() &&
		!submitting.value,
);

async function handleSubmit() {
	if (!canSubmit.value) return;
	submitting.value = true;
	try {
		const url = isEdit.value ? `/api/provider-nodes/${props.node?.id}` : "/api/provider-nodes";
		const method = isEdit.value ? "PUT" : "POST";
		const payload: Record<string, any> = {
			name: formData.value.name,
			prefix: formData.value.prefix,
			baseUrl: formData.value.baseUrl,
		};
		if (!isEdit.value) payload.type = "custom-embedding";

		const res = await fetch(url, {
			method,
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(payload),
		});
		const data = await res.json();
		if (res.ok) {
			if (isEdit.value) emit("saved", data.node);
			else emit("created", data.node);
		}
	} catch (error) {
		console.log("Error saving custom embedding node:", error);
	} finally {
		submitting.value = false;
	}
}

async function handleValidate() {
	validating.value = true;
	try {
		const res = await fetch("/api/provider-nodes/validate", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				baseUrl: formData.value.baseUrl,
				apiKey: checkKey.value,
				type: "custom-embedding",
				modelId: checkModelId.value.trim() || undefined,
			}),
		});
		validationResult.value = await res.json();
	} catch {
		validationResult.value = { valid: false, error: "Network error" };
	} finally {
		validating.value = false;
	}
}
</script>

<template>
  <Modal
    :is-open="props.isOpen"
    :title="isEdit ? 'Edit Custom Embedding' : 'Add Custom Embedding'"
    @close="emit('close')"
  >
    <div class="flex flex-col gap-4">
      <Input
        v-model="formData.name"
        label="Name"
        placeholder="Voyage AI"
        hint="Required. A friendly label for this embedding provider."
      />
      <Input
        v-model="formData.prefix"
        label="Prefix"
        placeholder="voyage"
        hint="Required. Used as the provider prefix for model IDs (e.g. voyage/voyage-3)."
      />
      <Input
        v-model="formData.baseUrl"
        label="Base URL"
        placeholder="https://api.voyageai.com/v1"
        hint="Most embedding APIs are OpenAI-compatible: Voyage, Cohere, Jina, Mistral, Together..."
      />
      <Input v-model="checkKey" label="API Key (for Check)" type="password" />
      <Input
        v-model="checkModelId"
        label="Model ID (for Check)"
        placeholder="e.g. voyage-3, embed-english-v3.0, text-embedding-3-small"
        hint="Required for validation. Will send a test embeddings request."
      />
      <div class="flex items-center gap-3">
        <Button
          :disabled="!checkKey || !checkModelId.trim() || validating || !formData.baseUrl.trim()"
          variant="secondary"
          @click="handleValidate"
        >
          {{ validating ? "Checking..." : "Check" }}
        </Button>
        <template v-if="validationResult">
          <template v-if="validationResult.valid">
            <Badge variant="success">Valid</Badge>
            <span v-if="validationResult.dimensions" class="text-sm text-text-muted">
              {{ validationResult.dimensions }} dims
            </span>
          </template>
          <div v-else class="flex flex-col gap-1">
            <Badge variant="error">Invalid</Badge>
            <span v-if="validationResult.error" class="text-sm text-red-500">{{ validationResult.error }}</span>
          </div>
        </template>
      </div>
      <div class="flex gap-2">
        <Button full-width :disabled="!canSubmit" @click="handleSubmit">
          {{ submitting ? (isEdit ? "Saving..." : "Creating...") : isEdit ? "Save" : "Create" }}
        </Button>
        <Button variant="ghost" full-width @click="emit('close')">Cancel</Button>
      </div>
    </div>
  </Modal>
</template>
