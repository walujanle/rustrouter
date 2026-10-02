<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import Select from "@/components/ui/UiSelect.vue";
import {
	isAnthropicCompatibleProvider,
	isOpenAICompatibleProvider,
	useProviders,
} from "@/constants/providers";

interface Connection {
	id?: string;
	name?: string;
	email?: string;
	priority?: number;
	authType?: string;
	provider?: string;
	providerSpecificData?: Record<string, any>;
}

const props = defineProps<{
	isOpen: boolean;
	connection?: Connection | null;
	proxyPools?: Array<Record<string, any>>;
}>();

const emit = defineEmits<{ save: [updates: Record<string, any>]; close: [] }>();

const { AI_PROVIDERS } = useProviders();

const formData = reactive({
	name: "",
	priority: 1,
	apiKey: "",
});
const region = ref("");
const testing = ref(false);
const testResult = ref<string | null>(null);
const validating = ref(false);
const validationResult = ref<string | null>(null);
const saving = ref(false);

watch(
	() => props.connection,
	(connection) => {
		if (!connection) return;
		formData.name = connection.name || "";
		formData.priority = connection.priority || 1;
		formData.apiKey = "";
		// Load region for providers that support it
		const providerCfg = AI_PROVIDERS?.[connection.provider ?? ""];
		if (providerCfg?.regions) {
			region.value =
				connection.providerSpecificData?.region ||
				providerCfg.defaultRegion ||
				providerCfg.regions[0]?.id ||
				"";
		}
		testResult.value = null;
		validationResult.value = null;
	},
	// The modal can be opened with a connection already set, so the first run
	// must populate the form rather than wait for a change.
	{ immediate: true },
);

const isOAuth = computed(() => props.connection?.authType === "oauth");
const isCompatible = computed(() =>
	props.connection
		? isOpenAICompatibleProvider(props.connection.provider) ||
			isAnthropicCompatibleProvider(props.connection.provider)
		: false,
);
const providerRegions = computed(() =>
	props.connection ? AI_PROVIDERS?.[props.connection.provider ?? ""]?.regions || null : null,
);
const regionOptions = computed(() =>
	(providerRegions.value || []).map((r: Record<string, any>) => ({
		value: r.id,
		label: r.label,
	})),
);

// Build providerSpecificData for region-aware providers
function buildRegionSpecificData() {
	if (providerRegions.value && region.value) {
		return { ...(props.connection?.providerSpecificData || {}), region: region.value };
	}
	return undefined;
}

function validationBody() {
	return {
		provider: props.connection?.provider,
		apiKey: formData.apiKey,
		...(providerRegions.value ? { providerSpecificData: buildRegionSpecificData() } : {}),
	};
}

async function handleTest() {
	if (!props.connection?.provider) return;
	testing.value = true;
	testResult.value = null;
	try {
		const res = await fetch(`/api/providers/${props.connection.id}/test`, { method: "POST" });
		const data = await res.json();
		testResult.value = data.valid ? "success" : "failed";
	} catch {
		testResult.value = "failed";
	} finally {
		testing.value = false;
	}
}

async function handleValidate() {
	if (!props.connection?.provider || !formData.apiKey) return;
	validating.value = true;
	validationResult.value = null;
	try {
		const res = await fetch("/api/providers/validate", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(validationBody()),
		});
		const data = await res.json();
		validationResult.value = data.valid ? "success" : "failed";
	} catch {
		validationResult.value = "failed";
	} finally {
		validating.value = false;
	}
}

async function handleSubmit() {
	if (!props.connection) return;
	saving.value = true;
	try {
		const updates: Record<string, any> = {
			name: formData.name,
			priority: formData.priority,
		};
		if (!isOAuth.value && formData.apiKey) {
			updates.apiKey = formData.apiKey;
			let isValid = validationResult.value === "success";
			if (!isValid) {
				try {
					validating.value = true;
					validationResult.value = null;
					const res = await fetch("/api/providers/validate", {
						method: "POST",
						headers: { "Content-Type": "application/json" },
						body: JSON.stringify(validationBody()),
					});
					const data = await res.json();
					isValid = !!data.valid;
					validationResult.value = isValid ? "success" : "failed";
				} catch {
					validationResult.value = "failed";
				} finally {
					validating.value = false;
				}
			}
			if (isValid) {
				updates.testStatus = "active";
				updates.lastError = null;
				updates.lastErrorAt = null;
			}
		}

		// Persist updated region for region-aware providers
		if (providerRegions.value && region.value) {
			updates.providerSpecificData = buildRegionSpecificData();
		}

		emit("save", updates);
	} finally {
		saving.value = false;
	}
}
</script>

<template>
  <Modal v-if="props.connection" :is-open="props.isOpen" title="Edit Connection" @close="emit('close')">
    <div class="flex flex-col gap-4">
      <Input
        v-model="formData.name"
        label="Name"
        :placeholder="isOAuth ? 'Account name' : 'Production Key'"
      />
      <div v-if="isOAuth && props.connection.email" class="bg-sidebar/50 p-3 rounded-lg">
        <p class="text-sm text-text-muted mb-1">Email</p>
        <p class="font-medium">{{ props.connection.email }}</p>
      </div>
      <Input
        :model-value="formData.priority"
        label="Priority"
        type="number"
        @update:model-value="formData.priority = Number.parseInt($event, 10) || 1"
      />

      <template v-if="!isOAuth">
        <div class="flex gap-2">
          <Input
            v-model="formData.apiKey"
            label="API Key"
            type="password"
            placeholder="Enter new API key"
            hint="Leave blank to keep the current API key."
            class="flex-1"
          />
          <div class="pt-6">
            <Button
              variant="secondary"
              :disabled="!formData.apiKey || validating || saving"
              @click="handleValidate"
            >
              {{ validating ? "Checking..." : "Check" }}
            </Button>
          </div>
        </div>
        <Badge v-if="validationResult" :variant="validationResult === 'success' ? 'success' : 'error'">
          {{ validationResult === "success" ? "Valid" : "Invalid" }}
        </Badge>
      </template>

      <Select
        v-if="providerRegions"
        v-model="region"
        label="Region"
        :options="regionOptions"
      />

      <div v-if="!isCompatible" class="flex items-center gap-3">
        <Button variant="secondary" :disabled="testing" @click="handleTest">
          {{ testing ? "Testing..." : "Test Connection" }}
        </Button>
        <Badge v-if="testResult" :variant="testResult === 'success' ? 'success' : 'error'">
          {{ testResult === "success" ? "Valid" : "Failed" }}
        </Badge>
      </div>

      <div class="flex gap-2">
        <Button full-width :disabled="saving" @click="handleSubmit">{{ saving ? "Saving..." : "Save" }}</Button>
        <Button variant="ghost" full-width @click="emit('close')">Cancel</Button>
      </div>
    </div>
  </Modal>
</template>
