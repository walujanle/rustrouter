<script setup lang="ts">
import { computed, ref, watch } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import Select from "@/components/ui/UiSelect.vue";

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		node?: Record<string, any> | null;
		isAnthropic?: boolean;
	}>(),
	{ node: null, isAnthropic: false },
);

const emit = defineEmits<{
	save: [payload: Record<string, any>];
	close: [];
}>();

const formData = ref({
	name: "",
	prefix: "",
	apiType: "chat",
	baseUrl: "https://api.openai.com/v1",
});
const saving = ref(false);
const checkKey = ref("");
const checkModelId = ref("");
const validating = ref(false);
const validationResult = ref<string | null>(null);

const apiTypeOptions = [
	{ value: "chat", label: "Chat Completions" },
	{ value: "responses", label: "Responses API" },
];

watch(
	() => [props.node, props.isAnthropic] as const,
	() => {
		if (!props.node) return;
		formData.value = {
			name: props.node.name || "",
			prefix: props.node.prefix || "",
			apiType: props.node.apiType || "chat",
			baseUrl:
				props.node.baseUrl ||
				(props.isAnthropic ? "https://api.anthropic.com/v1" : "https://api.openai.com/v1"),
		};
	},
	{ immediate: true },
);

const canSave = computed(
	() =>
		!!formData.value.name.trim() &&
		!!formData.value.prefix.trim() &&
		!!formData.value.baseUrl.trim() &&
		!saving.value,
);

async function handleSubmit() {
	if (!formData.value.name.trim() || !formData.value.prefix.trim() || !formData.value.baseUrl.trim())
		return;
	saving.value = true;
	try {
		const payload: Record<string, any> = {
			name: formData.value.name,
			prefix: formData.value.prefix,
			baseUrl: formData.value.baseUrl,
		};
		if (!props.isAnthropic) {
			payload.apiType = formData.value.apiType;
		}
		emit("save", payload);
	} finally {
		saving.value = false;
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
				type: props.isAnthropic ? "anthropic-compatible" : "openai-compatible",
				modelId: checkModelId.value.trim() || undefined,
			}),
		});
		const data = await res.json();
		validationResult.value = data.valid ? "success" : "failed";
	} catch {
		validationResult.value = "failed";
	} finally {
		validating.value = false;
	}
}
</script>

<template>
  <Modal
    v-if="props.node"
    :is-open="props.isOpen"
    :title="`Edit ${props.isAnthropic ? 'Anthropic' : 'OpenAI'} Compatible`"
    @close="emit('close')"
  >
    <div class="flex flex-col gap-4">
      <Input
        v-model="formData.name"
        label="Name"
        :placeholder="`${props.isAnthropic ? 'Anthropic' : 'OpenAI'} Compatible (Prod)`"
        hint="Required. A friendly label for this node."
      />
      <Input
        v-model="formData.prefix"
        label="Prefix"
        :placeholder="props.isAnthropic ? 'ac-prod' : 'oc-prod'"
        hint="Required. Used as the provider prefix for model IDs."
      />
      <Select
        v-if="!props.isAnthropic"
        v-model="formData.apiType"
        label="API Type"
        :options="apiTypeOptions"
      />
      <Input
        v-model="formData.baseUrl"
        label="Base URL"
        :placeholder="props.isAnthropic ? 'https://api.anthropic.com/v1' : 'https://api.openai.com/v1'"
        :hint="`Use the base URL (ending in /v1) for your ${props.isAnthropic ? 'Anthropic' : 'OpenAI'}-compatible API.`"
      />
      <div class="flex gap-2">
        <Input
          v-model="checkKey"
          label="API Key (for Check)"
          type="password"
          class="flex-1"
        />
        <div class="pt-6">
          <Button
            :disabled="!checkKey || validating || !formData.baseUrl.trim()"
            variant="secondary"
            @click="handleValidate"
          >
            {{ validating ? "Checking..." : "Check" }}
          </Button>
        </div>
      </div>
      <Input
        v-model="checkModelId"
        label="Model ID (optional)"
        placeholder="e.g. my-model-id"
        hint="If provider lacks /models endpoint, enter a model ID to validate via chat/completions instead."
      />
      <Badge v-if="validationResult" :variant="validationResult === 'success' ? 'success' : 'error'">
        {{ validationResult === "success" ? "Valid" : "Invalid" }}
      </Badge>
      <div class="flex gap-2">
        <Button :disabled="!canSave" full-width @click="handleSubmit">
          {{ saving ? "Saving..." : "Save" }}
        </Button>
        <Button variant="ghost" full-width @click="emit('close')">Cancel</Button>
      </div>
    </div>
  </Modal>
</template>
