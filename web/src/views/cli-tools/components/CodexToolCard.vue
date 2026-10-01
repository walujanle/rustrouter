<script setup lang="ts">
import { computed, ref, watch } from "vue";
import ManualConfigModal from "@/components/ManualConfigModal.vue";
import ModelSelectModal from "@/components/ModelSelectModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import type { CliTool } from "@/constants/cliTools";
import ApiKeySelect from "./ApiKeySelect.vue";
import BaseUrlSelect from "./BaseUrlSelect.vue";
import { matchKnownEndpoint } from "./cliEndpointMatch";
import { rememberEndpoint } from "./cliEndpointPresets";
import { getCurrentCodexProviderSettings } from "./codexConfig";

interface ApiKey {
	key: string;
}

interface CodexStatus {
	installed?: boolean;
	has9Router?: boolean;
	error?: string;
	config?: string | null;
}

const props = withDefaults(
	defineProps<{
		tool: CliTool;
		isExpanded: boolean;
		onToggle?: () => void;
		baseUrl: string;
		apiKeys?: ApiKey[];
		activeProviders?: Array<Record<string, any>>;
		cloudEnabled?: boolean;
		initialStatus?: CodexStatus | null;
	}>(),
	{
		onToggle: () => {},
		apiKeys: () => [],
		activeProviders: () => [],
		cloudEnabled: false,
		initialStatus: null,
	},
);

const codexStatus = ref<CodexStatus | null>(props.initialStatus || null);
const checkingCodex = ref(false);
const applying = ref(false);
const restoring = ref(false);
const message = ref<{ type: "success" | "error"; text: string } | null>(null);
const showInstallGuide = ref(false);
const selectedApiKey = ref("");
const selectedModel = ref("");
const subagentModel = ref("");
const modalOpen = ref(false);
const subagentModalOpen = ref(false);
const modelAliases = ref<Record<string, string>>({});
const showManualConfigModal = ref(false);
const customBaseUrl = ref("");

watch(
	() => [props.apiKeys, codexStatus.value?.config],
	() => {
		if (
			props.apiKeys &&
			props.apiKeys.length > 0 &&
			!selectedApiKey.value &&
			!codexStatus.value?.config
		) {
			selectedApiKey.value = props.apiKeys[0].key;
		}
	},
	{ immediate: true, deep: true },
);

watch(
	() => props.initialStatus,
	(initialStatus) => {
		if (initialStatus) codexStatus.value = initialStatus;
	},
);

watch(
	() => props.isExpanded,
	(isExpanded) => {
		if (isExpanded) {
			if (!codexStatus.value) checkCodexStatus();
			fetchModelAliases();
		}
	},
	{ immediate: true },
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

// Sync only when config content changes so local form edits are retained.
watch(
	() => codexStatus.value?.config,
	(config) => {
		if (config) {
			const { baseUrl, apiKey } = getCurrentCodexProviderSettings(config);
			customBaseUrl.value = baseUrl;
			selectedApiKey.value = apiKey;

			const modelMatch = config.match(/^model\s*=\s*"([^"]+)"/m);
			if (modelMatch) selectedModel.value = modelMatch[1];

			// Parse subagent settings
			const subagentModelMatch = config.match(/^default_subagent_model\s*=\s*"([^"]+)"/m);
			if (subagentModelMatch) subagentModel.value = subagentModelMatch[1];
		}
	},
);

const currentBaseUrl = computed(
	() => getCurrentCodexProviderSettings(codexStatus.value?.config).baseUrl,
);

const configStatus = computed(() => {
	if (!codexStatus.value?.installed) return null;
	if (!codexStatus.value.config) return "not_configured";
	return matchKnownEndpoint(currentBaseUrl.value) ? "configured" : "other";
});

function getEffectiveBaseUrl(): string {
	const url = (customBaseUrl.value || `${props.baseUrl}/v1`).replace(/\/+$/, "");
	// Ensure URL ends with /v1
	return url.endsWith("/v1") ? url : `${url}/v1`;
}

function getDisplayUrl(): string {
	return customBaseUrl.value || `${props.baseUrl}/v1`;
}

async function checkCodexStatus() {
	checkingCodex.value = true;
	try {
		const res = await fetch("/api/cli-tools/codex-settings", { cache: "no-store" });
		const data = await res.json();
		codexStatus.value = data;
	} catch (error) {
		codexStatus.value = { installed: false, error: (error as Error).message };
	} finally {
		checkingCodex.value = false;
	}
}

async function handleApplySettings() {
	applying.value = true;
	message.value = null;
	try {
		// Use sk_9router for localhost if no key, otherwise use selected key
		const keyToUse =
			selectedApiKey.value?.trim()
				? selectedApiKey.value
				: !props.cloudEnabled
					? "sk_9router"
					: selectedApiKey.value;

		const res = await fetch("/api/cli-tools/codex-settings", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				baseUrl: getEffectiveBaseUrl(),
				apiKey: keyToUse,
				model: selectedModel.value,
				subagentModel: subagentModel.value || selectedModel.value,
			}),
		});
		const data = await res.json();
		if (res.ok) {
			// Remember the endpoint so it stays selectable next time
			rememberEndpoint(getEffectiveBaseUrl());
			message.value = { type: "success", text: "Settings applied successfully!" };
			checkCodexStatus();
		} else {
			message.value = { type: "error", text: data.error || "Failed to apply settings" };
		}
	} catch (error) {
		message.value = { type: "error", text: (error as Error).message };
	} finally {
		applying.value = false;
	}
}

async function handleResetSettings() {
	restoring.value = true;
	message.value = null;
	try {
		const res = await fetch("/api/cli-tools/codex-settings", { method: "DELETE" });
		const data = await res.json();
		if (res.ok) {
			message.value = { type: "success", text: "Settings reset successfully!" };
			selectedModel.value = "";
			subagentModel.value = "";
			checkCodexStatus();
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
	selectedModel.value = model.value;
	// Auto-set subagent model if not set
	if (!subagentModel.value) {
		subagentModel.value = model.value;
	}
	modalOpen.value = false;
}

function getManualConfigs() {
	const keyToUse =
		selectedApiKey.value?.trim()
			? selectedApiKey.value
			: !props.cloudEnabled
				? "sk_9router"
				: "<API_KEY_FROM_DASHBOARD>";

	const effectiveSubagentModel = subagentModel.value || selectedModel.value;

	const configContent = `# RustRouter Configuration for Codex CLI
model = "${selectedModel.value}"
model_provider = "9router"

[model_providers.9router]
name = "RustRouter"
base_url = "${getEffectiveBaseUrl()}"
wire_api = "responses"

[model_providers.9router.http_headers]
Authorization = "Bearer ${keyToUse}"

[agents]
default_subagent_model = "${effectiveSubagentModel}"
`;

	return [
		{
			filename: "~/.codex/config.toml",
			content: configContent,
		},
	];
}
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
            src="/providers/codex.png"
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
      <div v-if="checkingCodex" class="flex items-center gap-2 text-text-muted">
        <span class="material-symbols-outlined animate-spin">progress_activity</span>
        <span>Checking Codex CLI...</span>
      </div>

      <div v-if="!checkingCodex && codexStatus && !codexStatus.installed" class="flex flex-col gap-4">
        <div class="flex flex-col gap-3 p-4 bg-yellow-500/10 border border-yellow-500/30 rounded-lg">
          <div class="flex items-start gap-3">
            <span class="material-symbols-outlined text-yellow-500">warning</span>
            <div class="flex-1">
              <p class="font-medium text-yellow-600 dark:text-yellow-400">Codex CLI not detected locally</p>
              <p class="text-sm text-text-muted">Manual configuration is still available if RustRouter is deployed on a remote server.</p>
            </div>
          </div>
          <div class="flex items-center gap-2 pl-9">
            <Button variant="secondary" size="sm" class="bg-yellow-500/20! border-yellow-500/40! text-yellow-700! dark:text-yellow-300! hover:bg-yellow-500/30!" @click="showManualConfigModal = true">
              <span class="material-symbols-outlined text-[18px] mr-1">content_copy</span>
              Manual Config
            </Button>
            <Button variant="outline" size="sm" @click="showInstallGuide = !showInstallGuide">
              <span class="material-symbols-outlined text-[18px] mr-1">{{ showInstallGuide ? "expand_less" : "help" }}</span>
              {{ showInstallGuide ? "Hide" : "How to Install" }}
            </Button>
          </div>
        </div>
        <div v-if="showInstallGuide" class="p-4 bg-surface border border-border rounded-lg">
          <h4 class="font-medium mb-3">Installation Guide</h4>
          <div class="space-y-3 text-sm">
            <div>
              <p class="text-text-muted mb-1">macOS / Linux / Windows:</p>
              <code class="block px-3 py-2 bg-black/5 dark:bg-white/5 rounded font-mono text-xs">npm install -g @openai/codex</code>
            </div>
            <p class="text-text-muted">After installation, run <code class="px-1 bg-black/5 dark:bg-white/5 rounded">codex</code> to verify.</p>
            <div class="pt-2 border-t border-border">
              <p class="text-text-muted text-xs">
                Codex reads custom providers from <code class="px-1 bg-black/5 dark:bg-white/5 rounded">~/.codex/config.toml</code>.
                Click &quot;Apply&quot; to auto-configure.
              </p>
            </div>
          </div>
        </div>
      </div>

      <template v-if="!checkingCodex && codexStatus?.installed">
        <div class="flex flex-col gap-2">
          <!-- Endpoint (selector) -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Select Endpoint</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <BaseUrlSelect
              :model-value="customBaseUrl || getDisplayUrl()"
              :requires-external-url="props.tool.requiresExternalUrl"
              :current-url="currentBaseUrl"
              @update:model-value="customBaseUrl = $event"
            />
          </div>

          <!-- Current configured -->
          <div v-if="codexStatus?.config && currentBaseUrl" class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Current</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <span class="min-w-0 truncate rounded bg-surface/40 px-2 py-2 text-xs text-text-muted sm:py-1.5">
              {{ currentBaseUrl }}
            </span>
          </div>

          <!-- API Key -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">API Key</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <ApiKeySelect :model-value="selectedApiKey" :api-keys="props.apiKeys" :cloud-enabled="props.cloudEnabled" @update:model-value="selectedApiKey = $event" />
          </div>

          <!-- Model -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Model</span>
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
              :disabled="!props.activeProviders?.length"
              :class="`w-full sm:w-auto rounded border px-2 py-2 text-xs transition-colors sm:py-1.5 whitespace-nowrap sm:shrink-0 ${props.activeProviders?.length ? 'bg-surface border-border text-text-main hover:border-primary cursor-pointer' : 'opacity-50 cursor-not-allowed border-border'}`"
              @click="modalOpen = true"
            >
              Select Model
            </button>
          </div>

          <!-- Subagent Model -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Subagent Model</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <div class="relative w-full min-w-0">
              <input
                type="text"
                :value="subagentModel"
                :placeholder="selectedModel || 'provider/model-id (defaults to main model)'"
                class="w-full min-w-0 pl-2 pr-7 py-2 bg-surface rounded border border-border text-xs focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
                @input="subagentModel = ($event.target as HTMLInputElement).value"
              />
              <button
                v-if="subagentModel"
                type="button"
                title="Clear (will use main model)"
                class="absolute right-1 top-1/2 -translate-y-1/2 p-0.5 text-text-muted hover:text-red-500 rounded transition-colors"
                @click="subagentModel = ''"
              >
                <span class="material-symbols-outlined text-[14px]">close</span>
              </button>
            </div>
            <button
              type="button"
              :disabled="!props.activeProviders?.length"
              :class="`w-full sm:w-auto rounded border px-2 py-2 text-xs transition-colors sm:py-1.5 whitespace-nowrap sm:shrink-0 ${props.activeProviders?.length ? 'bg-surface border-border text-text-main hover:border-primary cursor-pointer' : 'opacity-50 cursor-not-allowed border-border'}`"
              @click="subagentModalOpen = true"
            >
              Select Model
            </button>
          </div>
        </div>

        <div v-if="message" :class="`flex items-center gap-2 px-2 py-1.5 rounded text-xs ${message.type === 'success' ? 'bg-green-500/10 text-green-600' : 'bg-red-500/10 text-red-600'}`">
          <span class="material-symbols-outlined text-[14px]">{{ message.type === "success" ? "check_circle" : "error" }}</span>
          <span>{{ message.text }}</span>
        </div>

        <div class="grid grid-cols-1 gap-2 sm:flex sm:items-center">
          <Button
            variant="primary"
            size="sm"
            :disabled="(!selectedApiKey && props.cloudEnabled && (props.apiKeys?.length ?? 0) > 0) || !selectedModel"
            :loading="applying"
            @click="handleApplySettings"
          >
            <span class="material-symbols-outlined text-[14px] mr-1">save</span>Apply
          </Button>
          <Button variant="outline" size="sm" :disabled="restoring" :loading="restoring" @click="handleResetSettings">
            <span class="material-symbols-outlined text-[14px] mr-1">restore</span>Reset
          </Button>
          <Button variant="ghost" size="sm" @click="showManualConfigModal = true">
            <span class="material-symbols-outlined text-[14px] mr-1">content_copy</span>Manual Config
          </Button>
        </div>
      </template>
    </div>

    <ModelSelectModal
      v-if="modalOpen"
      :is-open="modalOpen"
      :selected-model="selectedModel"
      :active-providers="props.activeProviders"
      :model-aliases="modelAliases"
      title="Select Model for Codex"
      @close="modalOpen = false"
      @select="handleModelSelect"
    />

    <ModelSelectModal
      v-if="subagentModalOpen"
      :is-open="subagentModalOpen"
      :selected-model="subagentModel"
      :active-providers="props.activeProviders"
      :model-aliases="modelAliases"
      title="Select Subagent Model for Codex"
      @close="subagentModalOpen = false"
      @select="(model: any) => { subagentModel = model.value; subagentModalOpen = false; }"
    />

    <ManualConfigModal
      :is-open="showManualConfigModal"
      title="Codex CLI - Manual Configuration"
      :configs="getManualConfigs()"
      @close="showManualConfigModal = false"
    />
  </Card>
</template>
