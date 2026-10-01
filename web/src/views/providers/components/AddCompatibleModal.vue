<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import Select from "@/components/ui/UiSelect.vue";

const VARIANT_CONFIG = {
	openai: {
		title: "Add OpenAI Compatible",
		type: "openai-compatible",
		defaultBaseUrl: "https://api.openai.com/v1",
		namePlaceholder: "OpenAI Compatible (Prod)",
		prefixPlaceholder: "oc-prod",
		baseUrlHint: "Use the base URL (ending in /v1) for your OpenAI-compatible API.",
		modelIdPlaceholder: "e.g. gpt-4, claude-3-opus",
		errorLabel: "OpenAI Compatible",
		hasApiType: true,
	},
	anthropic: {
		title: "Add Anthropic Compatible",
		type: "anthropic-compatible",
		defaultBaseUrl: "https://api.anthropic.com/v1",
		namePlaceholder: "Anthropic Compatible (Prod)",
		prefixPlaceholder: "ac-prod",
		baseUrlHint:
			"Use the base URL (ending in /v1) for your Anthropic-compatible API. The system will append /messages.",
		modelIdPlaceholder: "e.g. claude-3-opus",
		errorLabel: "Anthropic Compatible",
		hasApiType: false,
	},
} as const;

const API_TYPE_OPTIONS = [
	{ value: "chat", label: "Chat Completions" },
	{ value: "responses", label: "Responses API" },
];

const props = defineProps<{
	variant: "openai" | "anthropic";
	isOpen: boolean;
}>();

const emit = defineEmits<{
	close: [];
	created: [node: Record<string, any>];
}>();

const config = computed(() => VARIANT_CONFIG[props.variant]);

function initialFormData(): Record<string, any> {
	return {
		name: "",
		prefix: "",
		...(config.value.hasApiType ? { apiType: "chat" } : {}),
		baseUrl: config.value.defaultBaseUrl,
	};
}

const formData = reactive<Record<string, any>>(initialFormData());
const submitting = ref(false);
const checkKey = ref("");
const checkModelId = ref("");
const validating = ref(false);
const validationResult = ref<Record<string, any> | null>(null);

// openai: reset baseUrl when apiType changes; anthropic: reset checks when opened
watch(
	() => (config.value.hasApiType ? formData.apiType : props.isOpen),
	() => {
		if (config.value.hasApiType) {
			formData.baseUrl = config.value.defaultBaseUrl;
		} else if (props.isOpen) {
			validationResult.value = null;
			checkKey.value = "";
			checkModelId.value = "";
		}
	},
);

async function handleSubmit() {
	if (
		!formData.name.trim() ||
		!formData.prefix.trim() ||
		!formData.baseUrl.trim()
	)
		return;
	submitting.value = true;
	try {
		const res = await fetch("/api/provider-nodes", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				name: formData.name,
				prefix: formData.prefix,
				...(config.value.hasApiType ? { apiType: formData.apiType } : {}),
				baseUrl: formData.baseUrl,
				type: config.value.type,
			}),
		});
		const data = await res.json();
		if (res.ok) {
			emit("created", data.node);
			Object.assign(formData, initialFormData());
			checkKey.value = "";
			validationResult.value = null;
		}
	} catch (error) {
		console.log(`Error creating ${config.value.errorLabel} node:`, error);
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
				baseUrl: formData.baseUrl,
				apiKey: checkKey.value,
				type: config.value.type,
				modelId: checkModelId.value.trim() || undefined,
			}),
		});
		const data = await res.json();
		validationResult.value = data;
	} catch {
		validationResult.value = { valid: false, error: "Network error" };
	} finally {
		validating.value = false;
	}
}
</script>

<template>
  <Modal :is-open="props.isOpen" :title="config.title" @close="emit('close')">
    <div class="flex flex-col gap-4">
      <Input
        v-model="formData.name"
        label="Name"
        :placeholder="config.namePlaceholder"
        hint="Required. A friendly label for this node."
      />
      <Input
        v-model="formData.prefix"
        label="Prefix"
        :placeholder="config.prefixPlaceholder"
        hint="Required. Used as the provider prefix for model IDs."
      />
      <Select
        v-if="config.hasApiType"
        v-model="formData.apiType"
        label="API Type"
        :options="API_TYPE_OPTIONS"
      />
      <Input
        v-model="formData.baseUrl"
        label="Base URL"
        :placeholder="config.defaultBaseUrl"
        :hint="config.baseUrlHint"
      />
      <Input
        v-model="checkKey"
        label="API Key (for Check)"
        type="password"
      />
      <Input
        v-model="checkModelId"
        label="Model ID (optional)"
        :placeholder="config.modelIdPlaceholder"
        hint="If provider lacks /models endpoint, enter a model ID to validate via chat/completions instead."
      />
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center">
        <Button
          :disabled="!checkKey || validating || !formData.baseUrl.trim()"
          variant="secondary"
          class="w-full sm:w-auto"
          @click="handleValidate"
        >
          {{ validating ? "Checking..." : "Check" }}
        </Button>
        <template v-if="validationResult">
          <template v-if="validationResult.valid">
            <Badge variant="success">Valid</Badge>
            <span v-if="validationResult.method === 'chat'" class="text-sm text-text-muted">
              (via inference test)
            </span>
          </template>
          <div v-else class="flex flex-col gap-1">
            <Badge variant="error">Invalid</Badge>
            <span v-if="validationResult.error" class="text-sm text-red-500">
              {{ validationResult.error }}
            </span>
          </div>
        </template>
      </div>
      <div class="flex flex-col gap-2 sm:flex-row">
        <Button
          full-width
          :disabled="
            !formData.name.trim() ||
            !formData.prefix.trim() ||
            !formData.baseUrl.trim() ||
            submitting
          "
          @click="handleSubmit"
        >
          {{ submitting ? "Creating..." : "Create" }}
        </Button>
        <Button variant="ghost" full-width @click="emit('close')">Cancel</Button>
      </div>
    </div>
  </Modal>
</template>
