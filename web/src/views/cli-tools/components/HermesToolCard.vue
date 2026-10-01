<script setup lang="ts">
import { computed, ref, watch } from "vue";
import ManualConfigModal from "@/components/ManualConfigModal.vue";
import ModelSelectModal from "@/components/ModelSelectModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import { CLI_TOOLS, type CliTool } from "@/constants/cliTools";
import ApiKeySelect from "./ApiKeySelect.vue";
import BaseUrlSelect from "./BaseUrlSelect.vue";
import { matchKnownEndpoint } from "./cliEndpointMatch";
import { rememberEndpoint } from "./cliEndpointPresets";

const ENDPOINT = "/api/cli-tools/hermes-settings";
const HERMES_ROLES = CLI_TOOLS.hermes?.roles || [];

interface ApiKey {
	key: string;
}

interface HermesStatus {
	installed?: boolean;
	has9Router?: boolean;
	error?: string;
	settings?: {
		model?: { base_url?: string; default?: string } | null;
		delegation?: { model?: string } | null;
		auxiliary?: Record<string, { model?: string }>;
	} | null;
}

const props = withDefaults(
	defineProps<{
		tool: CliTool;
		isExpanded: boolean;
		onToggle?: () => void;
		baseUrl: string;
		hasActiveProviders?: boolean;
		apiKeys?: ApiKey[];
		activeProviders?: Array<Record<string, any>>;
		cloudEnabled?: boolean;
		initialStatus?: HermesStatus | null;
	}>(),
	{
		onToggle: () => {},
		hasActiveProviders: false,
		apiKeys: () => [],
		activeProviders: () => [],
		cloudEnabled: false,
		initialStatus: null,
	},
);

const hermesStatus = ref<HermesStatus | null>(props.initialStatus || null);
const checking = ref(false);
const applying = ref(false);
const restoring = ref(false);
const message = ref<{ type: "success" | "error"; text: string } | null>(null);
const selectedApiKey = ref("");
const selectedModel = ref("");
const roleModels = ref<Record<string, string>>({});
const modalTarget = ref("default");
const modalOpen = ref(false);
const modelAliases = ref<Record<string, string>>({});
const showManualConfigModal = ref(false);
const customBaseUrl = ref("");
const hasInitializedModel = ref(false);

watch(
	() => props.apiKeys,
	(apiKeys) => {
		if (apiKeys && apiKeys.length > 0 && !selectedApiKey.value) {
			selectedApiKey.value = apiKeys[0].key;
		}
	},
	{ immediate: true, deep: true },
);

watch(
	() => props.initialStatus,
	(initialStatus) => {
		if (initialStatus) hermesStatus.value = initialStatus;
	},
);

watch(
	() => props.isExpanded,
	(isExpanded) => {
		if (isExpanded) {
			if (!hermesStatus.value) checkStatus();
			fetchModelAliases();
		}
	},
	{ immediate: true },
);

watch(
	() => hermesStatus.value,
	(status) => {
		if (status?.installed && !hasInitializedModel.value) {
			hasInitializedModel.value = true;
			const cfg = status.settings?.model;
			if (cfg?.default) selectedModel.value = cfg.default;
			const initial: Record<string, string> = {};
			if (status.settings?.delegation?.model) initial.delegation = status.settings.delegation.model;
			for (const [role, rcfg] of Object.entries(status.settings?.auxiliary || {})) {
				if (rcfg?.model) initial[role] = rcfg.model;
			}
			roleModels.value = initial;
		}
	},
);

async function fetchModelAliases() {
	try {
		const res = await fetch("/api/models/alias");
		const data = await res.json();
		if (res.ok) modelAliases.value = data.aliases || {};
	} catch (error) {
		console.log("Error fetching model aliases:", error);
	}
}

async function checkStatus() {
	checking.value = true;
	try {
		const res = await fetch(ENDPOINT);
		const data = await res.json();
		hermesStatus.value = data;
	} catch (error) {
		hermesStatus.value = { installed: false, error: (error as Error).message };
	} finally {
		checking.value = false;
	}
}

const currentBaseUrl = computed(() => hermesStatus.value?.settings?.model?.base_url || "");

const configStatus = computed(() => {
	if (!hermesStatus.value?.installed) return null;
	const cfg = hermesStatus.value.settings?.model;
	if (!cfg?.base_url) return "not_configured";
	return matchKnownEndpoint(cfg.base_url) ? "configured" : "other";
});

function normalizeLocalhost(url: string): string {
	return url.replace("://localhost", "://127.0.0.1");
}

function getLocalBaseUrl(): string {
	if (typeof window !== "undefined") {
		return normalizeLocalhost(window.location.origin);
	}
	return "http://127.0.0.1:20129";
}

function getEffectiveBaseUrl(): string {
	const url = customBaseUrl.value || getLocalBaseUrl();
	return url.endsWith("/v1") ? url : `${url}/v1`;
}

async function handleApply() {
	applying.value = true;
	message.value = null;
	try {
		const keyToUse =
			selectedApiKey.value?.trim() ||
			(props.apiKeys?.length > 0 ? props.apiKeys[0].key : null) ||
			(!props.cloudEnabled ? "sk_9router" : null);

		const res = await fetch(ENDPOINT, {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				baseUrl: getEffectiveBaseUrl(),
				apiKey: keyToUse,
				selections: [
					{ role: "default", model: selectedModel.value },
					...Object.entries(roleModels.value)
						.filter(([, model]) => model?.trim())
						.map(([role, model]) => ({ role, model: model.trim() })),
				],
			}),
		});
		const data = await res.json();
		if (res.ok) {
			// Remember the endpoint so it stays selectable next time
			rememberEndpoint(getEffectiveBaseUrl());
			message.value = { type: "success", text: "Settings applied successfully!" };
			checkStatus();
		} else {
			message.value = { type: "error", text: data.error || "Failed to apply settings" };
		}
	} catch (error) {
		message.value = { type: "error", text: (error as Error).message };
	} finally {
		applying.value = false;
	}
}

async function handleReset() {
	restoring.value = true;
	message.value = null;
	try {
		const res = await fetch(ENDPOINT, { method: "DELETE" });
		const data = await res.json();
		if (res.ok) {
			message.value = { type: "success", text: "Settings reset successfully!" };
			selectedModel.value = "";
			roleModels.value = {};
			checkStatus();
		} else {
			message.value = { type: "error", text: data.error || "Failed to reset settings" };
		}
	} catch (error) {
		message.value = { type: "error", text: (error as Error).message };
	} finally {
		restoring.value = false;
	}
}

function handleModelSelect(model: { value: string }) {
	if (modalTarget.value === "default") {
		selectedModel.value = model.value;
	} else {
		roleModels.value = { ...roleModels.value, [modalTarget.value]: model.value };
	}
	modalOpen.value = false;
}

function openModelModal(target: string) {
	modalTarget.value = target;
	modalOpen.value = true;
}

function getManualConfigs() {
	const keyToUse =
		selectedApiKey.value?.trim()
			? selectedApiKey.value
			: !props.cloudEnabled
				? "sk_9router"
				: "<API_KEY_FROM_DASHBOARD>";

	const base = getEffectiveBaseUrl();
	let yamlContent = `model:\n  default: "${selectedModel.value || "provider/model-id"}"\n  provider: "custom"\n  base_url: "${base}"\n  api_key: \${OPENAI_API_KEY}\n`;
	if (roleModels.value.delegation?.trim()) {
		yamlContent += `delegation:\n  model: "${roleModels.value.delegation.trim()}"\n  provider: "custom"\n  base_url: "${base}"\n  api_key: \${OPENAI_API_KEY}\n`;
	}
	const auxRoles = Object.entries(roleModels.value).filter(
		([role, model]) => role !== "delegation" && model?.trim(),
	);
	if (auxRoles.length > 0) {
		yamlContent += `auxiliary:\n${auxRoles
			.map(
				([role, model]) =>
					`  ${role}:\n    provider: "custom"\n    model: "${model.trim()}"\n    base_url: "${base}"\n    api_key: \${OPENAI_API_KEY}\n`,
			)
			.join("")}`;
	}
	const envContent = `OPENAI_API_KEY=${keyToUse}\n`;

	return [
		{ filename: "~/.hermes/config.yaml", content: yamlContent },
		{ filename: "~/.hermes/.env", content: envContent },
	];
}

const modalTitle = computed(() => {
	const suffix =
		modalTarget.value !== "default"
			? ` — ${HERMES_ROLES.find((r) => r.id === modalTarget.value)?.label || modalTarget.value}`
			: "";
	return `Select Model for Hermes Agent${suffix}`;
});
</script>

<template>
  <Card padding="xs" class="overflow-hidden">
    <button
      type="button"
      class="flex w-full items-start justify-between gap-3 hover:cursor-pointer sm:items-center"
      @click="props.onToggle"
    >
      <span class="flex min-w-0 items-center gap-3">
        <span class="size-8 flex items-center justify-center shrink-0">
          <img
            src="/providers/hermes.png"
            :alt="props.tool.name"
            width="32"
            height="32"
            class="size-8 object-contain rounded-lg"
            sizes="32px"
            loading="lazy"
            decoding="async"
            @error="($event.target as HTMLImageElement).style.display = 'none'"
          />
        </span>
        <span class="min-w-0">
          <span class="flex min-w-0 flex-wrap items-center gap-2">
            <span class="font-medium text-sm">{{ props.tool.name }}</span>
            <span v-if="configStatus === 'configured'" class="px-1.5 py-0.5 text-[10px] font-medium bg-green-500/10 text-green-600 dark:text-green-400 rounded-full">Connected</span>
            <span v-if="configStatus === 'not_configured'" class="px-1.5 py-0.5 text-[10px] font-medium bg-yellow-500/10 text-yellow-600 dark:text-yellow-400 rounded-full">Not configured</span>
            <span v-if="configStatus === 'other'" class="px-1.5 py-0.5 text-[10px] font-medium bg-blue-500/10 text-blue-600 dark:text-blue-400 rounded-full">Other</span>
          </span>
          <span class="block text-xs text-text-muted truncate">{{ props.tool.description }}</span>
        </span>
      </span>
      <span :class="`material-symbols-outlined text-text-muted text-[20px] transition-transform ${props.isExpanded ? 'rotate-180' : ''}`">expand_more</span>
    </button>

    <div v-if="props.isExpanded" class="mt-4 pt-4 border-t border-border flex flex-col gap-4">
      <div v-if="checking" class="flex items-center gap-2 text-text-muted">
        <span class="material-symbols-outlined animate-spin">progress_activity</span>
        <span>Checking Hermes Agent...</span>
      </div>

      <div v-if="!checking && hermesStatus && !hermesStatus.installed" class="flex flex-col gap-4">
        <div class="flex flex-col gap-3 p-4 bg-yellow-500/10 border border-yellow-500/30 rounded-lg">
          <div class="flex items-start gap-3">
            <span class="material-symbols-outlined text-yellow-500">warning</span>
            <div class="flex-1">
              <p class="font-medium text-yellow-600 dark:text-yellow-400">Hermes Agent not detected locally</p>
              <p class="text-sm text-text-muted">Install: curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash</p>
            </div>
          </div>
          <div class="flex flex-col sm:flex-row sm:items-center gap-2 pl-0 sm:pl-9">
            <Button variant="secondary" size="sm" class="w-full sm:w-auto !bg-yellow-500/20 !border-yellow-500/40 !text-yellow-700 dark:!text-yellow-300 hover:!bg-yellow-500/30" @click="showManualConfigModal = true">
              <span class="material-symbols-outlined text-[18px] mr-1">content_copy</span>
              Manual Config
            </Button>
          </div>
        </div>
      </div>

      <template v-if="!checking && hermesStatus?.installed">
        <div class="flex flex-col gap-2">
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Select Endpoint</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <BaseUrlSelect
              :model-value="customBaseUrl || getEffectiveBaseUrl()"
              :requires-external-url="props.tool.requiresExternalUrl"
              :current-url="currentBaseUrl"
              @update:model-value="customBaseUrl = $event"
            />
          </div>

          <div v-if="hermesStatus?.settings?.model?.base_url" class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Current</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <span class="min-w-0 truncate rounded bg-surface/40 px-2 py-2 text-xs text-text-muted sm:py-1.5">
              {{ hermesStatus.settings.model.base_url }}
            </span>
          </div>

          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">API Key</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <ApiKeySelect :model-value="selectedApiKey" :api-keys="props.apiKeys" :cloud-enabled="props.cloudEnabled" @update:model-value="selectedApiKey = $event" />
          </div>

          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Default Model</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <div class="relative w-full min-w-0">
              <input
                type="text"
                :value="selectedModel"
                placeholder="provider/model-id"
                class="w-full min-w-0 pl-2 pr-7 py-2 bg-surface rounded border border-border text-xs focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
                @input="selectedModel = ($event.target as HTMLInputElement).value"
              />
              <button
                v-if="selectedModel"
                type="button"
                title="Clear"
                class="absolute right-1 top-1/2 -translate-y-1/2 p-0.5 text-text-muted hover:text-red-500 rounded transition-colors"
                @click="selectedModel = ''"
              >
                <span class="material-symbols-outlined text-[14px]">close</span>
              </button>
            </div>
            <button
              type="button"
              :disabled="!props.hasActiveProviders"
              :class="`w-full sm:w-auto rounded border px-2 py-2 text-xs transition-colors sm:py-1.5 whitespace-nowrap sm:shrink-0 ${props.hasActiveProviders ? 'bg-surface border-border text-text-main hover:border-primary cursor-pointer' : 'opacity-50 cursor-not-allowed border-border'}`"
              @click="openModelModal('default')"
            >
              Select
            </button>
          </div>

          <details class="group">
            <summary class="cursor-pointer select-none text-xs font-semibold text-text-main hover:text-primary transition-colors">
              <span class="material-symbols-outlined align-middle text-[16px] text-text-muted group-open:rotate-90 transition-transform">chevron_right</span>
              Model Roles (optional)
            </summary>
            <div class="mt-2 flex flex-col gap-1.5">
              <div
                v-for="role in HERMES_ROLES"
                :key="role.id"
                class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2"
              >
                <span class="truncate text-xs font-semibold text-text-main sm:text-right sm:text-sm" :title="role.label">{{ role.label }}</span>
                <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
                <div class="relative w-full min-w-0">
                  <input
                    type="text"
                    :value="roleModels[role.id] || ''"
                    placeholder="inherit default"
                    class="w-full min-w-0 pl-2 pr-7 py-2 bg-surface rounded border border-border text-xs focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
                    @input="roleModels = { ...roleModels, [role.id]: ($event.target as HTMLInputElement).value }"
                  />
                  <button
                    v-if="roleModels[role.id]"
                    type="button"
                    title="Clear"
                    class="absolute right-1 top-1/2 -translate-y-1/2 p-0.5 text-text-muted hover:text-red-500 rounded transition-colors"
                    @click="roleModels = { ...roleModels, [role.id]: '' }"
                  >
                    <span class="material-symbols-outlined text-[14px]">close</span>
                  </button>
                </div>
                <button
                  type="button"
                  :disabled="!props.hasActiveProviders"
                  :class="`w-full sm:w-auto rounded border px-2 py-2 text-xs transition-colors sm:py-1.5 whitespace-nowrap sm:shrink-0 ${props.hasActiveProviders ? 'bg-surface border-border text-text-main hover:border-primary cursor-pointer' : 'opacity-50 cursor-not-allowed border-border'}`"
                  @click="openModelModal(role.id)"
                >
                  Select
                </button>
              </div>
              <p class="text-xs text-text-muted">Empty roles inherit the default model.</p>
            </div>
          </details>
        </div>

        <div v-if="message" :class="`flex items-center gap-2 px-2 py-1.5 rounded text-xs ${message.type === 'success' ? 'bg-green-500/10 text-green-600' : 'bg-red-500/10 text-red-600'}`">
          <span class="material-symbols-outlined text-[14px]">{{ message.type === "success" ? "check_circle" : "error" }}</span>
          <span>{{ message.text }}</span>
        </div>

        <div class="flex flex-col sm:flex-row sm:items-center gap-2">
          <Button variant="primary" size="sm" :disabled="!selectedModel" :loading="applying" class="w-full sm:w-auto" @click="handleApply">
            <span class="material-symbols-outlined text-[14px] mr-1">save</span>Apply
          </Button>
          <Button variant="outline" size="sm" :disabled="!hermesStatus?.has9Router" :loading="restoring" class="w-full sm:w-auto" @click="handleReset">
            <span class="material-symbols-outlined text-[14px] mr-1">restore</span>Reset
          </Button>
          <Button variant="ghost" size="sm" class="w-full sm:w-auto" @click="showManualConfigModal = true">
            <span class="material-symbols-outlined text-[14px] mr-1">content_copy</span>Manual Config
          </Button>
        </div>
      </template>
    </div>

    <ModelSelectModal
      v-if="modalOpen"
      :is-open="modalOpen"
      :selected-model="modalTarget === 'default' ? selectedModel : roleModels[modalTarget] || ''"
      :active-providers="props.activeProviders"
      :model-aliases="modelAliases"
      :title="modalTitle"
      @close="modalOpen = false"
      @select="handleModelSelect"
    />

    <ManualConfigModal
      :is-open="showManualConfigModal"
      title="Hermes Agent - Manual Configuration"
      :configs="getManualConfigs()"
      @close="showManualConfigModal = false"
    />
  </Card>
</template>
