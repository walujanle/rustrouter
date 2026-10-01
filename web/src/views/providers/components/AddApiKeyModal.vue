<script setup lang="ts">
import { computed, ref } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import Select from "@/components/ui/UiSelect.vue";
import { useProviders } from "@/constants/providers";
import { planBulkAdd } from "@/utils/bulkAdd";

const BULK_PLACEHOLDER = `name1|sk-key1\nname2|sk-key2\nsk-key-only-auto-named`;
const NONE_PROXY_POOL_VALUE = "__none__";

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		provider?: string;
		providerName?: string;
		isCompatible?: boolean;
		isAnthropic?: boolean;
		authType?: string;
		authHint?: string;
		website?: string;
		proxyPools?: Array<Record<string, any>>;
		error?: string;
		existingNames?: string[];
	}>(),
	{
		provider: "",
		providerName: "",
		isCompatible: false,
		isAnthropic: false,
		authType: "",
		authHint: "",
		website: "",
		proxyPools: () => [],
		error: "",
		existingNames: () => [],
	},
);

const emit = defineEmits<{
	save: [formData: Record<string, any>];
	bulkDone: [];
	close: [];
}>();

const { AI_PROVIDERS } = useProviders();

const isCookie = computed(() => props.authType === "cookie");
const credentialLabel = computed(() =>
	isCookie.value ? "Cookie Value" : "API Key",
);
const credentialPlaceholder = computed(() =>
	isCookie.value ? "eyJhbGciOi..." : "",
);

const providerRegions = computed(
	() => AI_PROVIDERS[props.provider]?.regions || null,
);
const defaultRegion = computed(
	() =>
		AI_PROVIDERS[props.provider]?.defaultRegion ||
		providerRegions.value?.[0]?.id ||
		"",
);

const formData = ref({
	name: "",
	apiKey: "",
	defaultModel: "",
	priority: 1,
	proxyPoolId: NONE_PROXY_POOL_VALUE,
});
const region = ref(defaultRegion.value);
const validating = ref(false);
const validationResult = ref<string | null>(null);
const saving = ref(false);
const mode = ref<"single" | "bulk">("single");
const bulkText = ref("");
const bulkResult = ref<{ success: number; failed: number } | null>(null);

const bulkPlaceholder = computed(() => BULK_PLACEHOLDER);

const regionOptions = computed(() =>
	(providerRegions.value || []).map((r: Record<string, any>) => ({
		value: r.id,
		label: r.label,
	})),
);

const proxyOptions = computed(() => [
	{ value: NONE_PROXY_POOL_VALUE, label: "None" },
	...(props.proxyPools || []).map((pool) => ({ value: pool.id, label: pool.name })),
]);

function buildProviderSpecificData() {
	if (providerRegions.value && region.value) {
		return { region: region.value };
	}
	return undefined;
}

async function handleValidate() {
	validating.value = true;
	try {
		const res = await fetch("/api/providers/validate", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				provider: props.provider,
				apiKey: formData.value.apiKey,
				providerSpecificData: buildProviderSpecificData(),
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

async function handleSubmit() {
	if (!props.provider) return;
	if (!formData.value.apiKey) return;
	if (!formData.value.name) return;
	if (props.isCompatible && !formData.value.defaultModel.trim()) return;

	saving.value = true;
	try {
		let isValid = false;
		try {
			validating.value = true;
			validationResult.value = null;
			const res = await fetch("/api/providers/validate", {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({
					provider: props.provider,
					apiKey: formData.value.apiKey,
					providerSpecificData: buildProviderSpecificData(),
				}),
			});
			const data = await res.json();
			isValid = !!data.valid;
			validationResult.value = isValid ? "success" : "failed";
		} catch {
			validationResult.value = "failed";
		} finally {
			validating.value = false;
		}

		emit("save", {
			name: formData.value.name || "",
			apiKey: formData.value.apiKey,
			defaultModel: props.isCompatible ? formData.value.defaultModel.trim() : undefined,
			priority: formData.value.priority,
			proxyPoolId:
				formData.value.proxyPoolId === NONE_PROXY_POOL_VALUE
					? null
					: formData.value.proxyPoolId,
			testStatus: isValid ? "active" : "unknown",
			providerSpecificData: buildProviderSpecificData(),
		});
	} finally {
		saving.value = false;
	}
}

async function handleBulkSubmit() {
	const lines = bulkText.value.split("\n");
	if (!lines.length) return;
	const plan = planBulkAdd(lines, props.existingNames);
	if (!plan.length) return;
	saving.value = true;
	bulkResult.value = null;
	let success = 0;
	let failed = 0;
	for (const entry of plan) {
		try {
			let isValid = false;
			try {
				const vres = await fetch("/api/providers/validate", {
					method: "POST",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify({ provider: props.provider, apiKey: entry.apiKey }),
				});
				const vdata = await vres.json().catch(() => ({}));
				isValid = !!vdata.valid;
			} catch {
				isValid = false;
			}
			const res = await fetch("/api/providers", {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({
					provider: props.provider,
					apiKey: entry.apiKey,
					name: entry.name,
					priority: 1,
					testStatus: isValid ? "active" : "unknown",
					...(entry.providerSpecificData
						? { providerSpecificData: entry.providerSpecificData }
						: {}),
				}),
			});
			if (res.ok) success++;
			else failed++;
		} catch {
			failed++;
		}
	}
	saving.value = false;
	bulkResult.value = { success, failed };
	if (success > 0) emit("bulkDone");
}

const canSave = computed(
	() =>
		!(saving.value || !formData.value.name || !formData.value.apiKey) &&
		!(props.isCompatible && !formData.value.defaultModel.trim()),
);
</script>

<template>
  <Modal
    v-if="props.provider"
    :is-open="props.isOpen"
    :title="`Add ${props.providerName || props.provider} ${credentialLabel}`"
    @close="emit('close')"
  >
    <div class="flex flex-col gap-4">
      <!-- Mode switcher -->
      <div class="flex gap-2">
        <Button size="sm" :variant="mode === 'single' ? 'primary' : 'ghost'" @click="() => { mode = 'single'; bulkResult = null; }">Single</Button>
        <Button size="sm" :variant="mode === 'bulk' ? 'primary' : 'ghost'" @click="() => { mode = 'bulk'; bulkResult = null; }">Bulk Add</Button>
      </div>

      <div v-if="mode === 'bulk'" class="flex flex-col gap-3">
        <p class="text-xs text-text-muted">
          One key per line. Format: <code>name|apiKey</code> or just <code>apiKey</code> (auto-named by index).
        </p>
        <textarea
          v-model="bulkText"
          class="w-full rounded border border-accent/30 bg-sidebar p-2 text-sm font-mono resize-y min-h-35 focus:outline-none focus:ring-1 focus:ring-primary"
          :placeholder="bulkPlaceholder"
        />
        <div
          v-if="bulkResult"
          :class="`text-sm font-medium ${bulkResult.failed > 0 ? 'text-yellow-400' : 'text-green-400'}`"
        >
          ✓ {{ bulkResult.success }} added{{ bulkResult.failed > 0 ? `, ✗ ${bulkResult.failed} failed` : "" }}
        </div>
        <div class="flex gap-2">
          <Button :disabled="saving || !bulkText.trim()" full-width @click="handleBulkSubmit">
            {{ saving ? "Adding..." : "Add All Keys" }}
          </Button>
          <Button variant="ghost" full-width @click="emit('close')">Cancel</Button>
        </div>
      </div>

      <template v-if="mode === 'single'">
        <Input
          v-model="formData.name"
          label="Name"
          placeholder="Production Key"
        />
        <div class="flex gap-2">
          <Input
            v-model="formData.apiKey"
            :label="credentialLabel"
            :type="isCookie ? 'text' : 'password'"
            :placeholder="credentialPlaceholder"
            class="flex-1"
          />
          <div class="pt-6">
            <Button :disabled="!formData.apiKey || validating || saving" variant="secondary" @click="handleValidate">
              {{ validating ? "Checking..." : "Check" }}
            </Button>
          </div>
        </div>
        <p v-if="isCookie && props.authHint" class="text-xs text-text-muted">
          {{ props.authHint }}
          <template v-if="props.website">
            {{ " " }}
            <a :href="props.website" target="_blank" rel="noopener noreferrer" class="text-primary underline">
              Open {{ props.website.replace(/^https?:\/\//, "") }}
            </a>
          </template>
        </p>
        <Select
          v-if="providerRegions"
          v-model="region"
          label="Region"
          :options="regionOptions"
        />
        <Input
          v-if="props.isCompatible"
          v-model="formData.defaultModel"
          label="Default Model"
          :placeholder="props.isAnthropic ? 'claude-3-5-sonnet-latest' : 'gpt-4o-mini'"
        />
        <Badge v-if="validationResult" :variant="validationResult === 'success' ? 'success' : 'error'">
          {{ validationResult === "success" ? "Valid" : "Invalid" }}
        </Badge>
        <p v-if="props.error" class="text-xs text-red-500 wrap-break-word">{{ props.error }}</p>
        <p v-if="props.isCompatible" class="text-xs text-text-muted">
          Enter the model ID exactly as your compatible endpoint expects it. This model will be saved as the connection default.
        </p>
        <Input
          v-model="formData.priority"
          label="Priority"
          type="number"
        />

        <Select
          v-model="formData.proxyPoolId"
          label="Proxy Pool"
          :options="proxyOptions"
          placeholder="None"
        />

        <p v-if="(props.proxyPools || []).length === 0" class="text-xs text-text-muted">
          No active proxy pools available. Create one in Proxy Pools page first.
        </p>

        <p class="text-xs text-text-muted">
          Legacy manual proxy fields are still accepted by API for backward compatibility.
        </p>

        <div class="flex gap-2">
          <Button :disabled="!canSave" full-width @click="handleSubmit">
            {{ saving ? "Saving..." : "Save" }}
          </Button>
          <Button variant="ghost" full-width @click="emit('close')">Cancel</Button>
        </div>
      </template>
    </div>
  </Modal>
</template>
