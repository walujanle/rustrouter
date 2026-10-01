<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import ManualConfigModal from "@/components/ManualConfigModal.vue";
import ModelSelectModal from "@/components/ModelSelectModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Tooltip from "@/components/ui/UiTooltip.vue";
import type { CliTool } from "@/constants/cliTools";
import ApiKeySelect from "./ApiKeySelect.vue";
import BaseUrlSelect from "./BaseUrlSelect.vue";
import { matchKnownEndpoint } from "./cliEndpointMatch";
import { rememberEndpoint } from "./cliEndpointPresets";

// Claude Code appends a bracketed context marker to the model name when the
// 1M-context beta is toggled on. The marker is a client-side annotation, not
// part of any model id — strip it before appending so repeated toggles cannot
// stack `[1m][1m]`.
const CONTEXT_MARKER = /\[1m\]$/i;
function stripModelContextMarker(modelStr: string): { model: string } {
	if (typeof modelStr !== "string") return { model: modelStr };
	const trimmed = modelStr.trim();
	const match = trimmed.match(CONTEXT_MARKER);
	if (!match) return { model: modelStr };
	return { model: trimmed.slice(0, -match[0].length) };
}

// Auto-compact window presets (CLAUDE_CODE_AUTO_COMPACT_WINDOW, valid 100K–1M).
// UI shows the round number; the value written is nudged down 2K to stay safely
// under the upstream hard cap.
const CONTEXT_OPTIONS = [
	{ label: "Default", value: "" },
	{ label: "200K", value: "198000" },
	{ label: "300K", value: "298000" },
	{ label: "500K", value: "498000" },
	{ label: "700K", value: "698000" },
];

interface ApiKey {
	key: string;
}

interface ClaudeStatus {
	installed?: boolean;
	has9Router?: boolean;
	hasBackup?: boolean;
	exaMcpEnabled?: boolean;
	error?: string;
	settings?: { env?: Record<string, string> };
}

const props = withDefaults(
	defineProps<{
		tool: CliTool;
		isExpanded: boolean;
		onToggle?: () => void;
		activeProviders?: Array<Record<string, any>>;
		modelMappings?: Record<string, string>;
		baseUrl: string;
		hasActiveProviders?: boolean;
		apiKeys?: ApiKey[];
		cloudEnabled?: boolean;
		initialStatus?: ClaudeStatus | null;
	}>(),
	{
		onToggle: () => {},
		activeProviders: () => [],
		modelMappings: () => ({}),
		hasActiveProviders: false,
		apiKeys: () => [],
		cloudEnabled: false,
		initialStatus: null,
	},
);

const emit = defineEmits<{ modelMappingChange: [alias: string, target: string] }>();

const claudeStatus = ref<ClaudeStatus | null>(props.initialStatus || null);
const checkingClaude = ref(false);
const applying = ref(false);
const restoring = ref(false);
const message = ref<{ type: "success" | "error"; text: string } | null>(null);
const showInstallGuide = ref(false);
const modalOpen = ref(false);
const currentEditingAlias = ref<string | null>(null);
const selectedApiKey = ref("");
const modelAliases = ref<Record<string, string>>({});
const showManualConfigModal = ref(false);
const customBaseUrl = ref("");
const ccFilterNaming = ref(false);
const exaMcpEnabled = ref(false);
const autoCompactWindow = ref("");
const oneMContext = ref(false);
let hasInitializedModels = false;

function onModelMappingChange(alias: string, target: string) {
	emit("modelMappingChange", alias, target);
}

// Claude Code only string-matches the marker against the model name, so it
// applies to any id — the user decides which models are worth declaring as 1M.
function withContextMarker(value: string, enabled: boolean): string {
	const { model } = stripModelContextMarker(value);
	return enabled ? `${model}[1m]` : model;
}

// Rewrite the mappings in place on toggle, so the inputs show what will be
// written without waiting for Apply.
function handleOneMContextToggle(enabled: boolean) {
	oneMContext.value = enabled;
	props.tool.defaultModels?.forEach((model) => {
		const current = props.modelMappings[model.alias];
		if (current) onModelMappingChange(model.alias, withContextMarker(current, enabled));
	});
}

const currentBaseUrl = computed(() => claudeStatus.value?.settings?.env?.ANTHROPIC_BASE_URL || "");

const configStatus = computed(() => {
	if (!claudeStatus.value?.installed) return null;
	const currentUrl = claudeStatus.value.settings?.env?.ANTHROPIC_BASE_URL;
	if (!currentUrl) return "not_configured";
	if (matchKnownEndpoint(currentUrl)) return "configured";
	return "other";
});

watch(
	() => props.apiKeys,
	(keys) => {
		if (keys && keys.length > 0 && !selectedApiKey.value) {
			selectedApiKey.value = keys[0].key;
		}
	},
	{ immediate: true, deep: true },
);

watch(
	() => props.initialStatus,
	(initialStatus) => {
		if (initialStatus) {
			claudeStatus.value = initialStatus;
			exaMcpEnabled.value = !!initialStatus.exaMcpEnabled;
		}
	},
);

watch(
	() => claudeStatus.value?.settings?.env?.CLAUDE_CODE_AUTO_COMPACT_WINDOW,
	(v) => {
		autoCompactWindow.value = v || "";
	},
);

watch(
	() => claudeStatus.value?.settings?.env,
	(env) => {
		if (!env) return;
		oneMContext.value = (props.tool.defaultModels || []).some((model) =>
			env[model.envKey as string]?.endsWith("[1m]"),
		);
	},
);

watch(
	() => props.isExpanded,
	(isExpanded) => {
		if (isExpanded) {
			if (!claudeStatus.value) checkClaudeStatus();
			fetchModelAliases();
		}
	},
	{ immediate: true },
);

watch(
	() => [claudeStatus.value, props.apiKeys, props.tool.defaultModels],
	() => {
		if (claudeStatus.value?.installed && !hasInitializedModels) {
			hasInitializedModels = true;
			const env = claudeStatus.value.settings?.env || {};

			props.tool.defaultModels?.forEach((model) => {
				if (model.envKey) {
					// Kept verbatim (marker included) so the input matches what is on disk;
					// withContextMarker strips before appending, so re-applying cannot double it.
					const value = env[model.envKey] || model.defaultValue || "";
					// Only sync initial values from file once
					if (value) {
						onModelMappingChange(model.alias, value);
					}
				}
			});
			// Restore key from settings.json; ApiKeySelect matches it against saved presets
			const tokenFromFile = env.ANTHROPIC_AUTH_TOKEN;
			if (tokenFromFile) {
				selectedApiKey.value = tokenFromFile;
			}
		}
	},
	{ immediate: true, deep: true },
);

onMounted(() => {
	fetch("/api/settings")
		.then((r) => r.json())
		.then((data) => {
			ccFilterNaming.value = !!data.ccFilterNaming;
		})
		.catch(() => {});
});

async function handleCcFilterNamingToggle(e: Event) {
	const value = (e.target as HTMLInputElement).checked;
	ccFilterNaming.value = value;
	await fetch("/api/settings", {
		method: "PATCH",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify({ ccFilterNaming: value }),
	}).catch(() => {});
}

async function fetchModelAliases() {
	try {
		const res = await fetch("/api/models/alias");
		const data = await res.json();
		if (res.ok) modelAliases.value = data.aliases || {};
	} catch (error) {
		console.log("Error fetching model aliases:", error);
	}
}

async function checkClaudeStatus() {
	checkingClaude.value = true;
	try {
		const res = await fetch("/api/cli-tools/claude-settings");
		const data = await res.json();
		claudeStatus.value = data;
		exaMcpEnabled.value = !!data.exaMcpEnabled;
	} catch (error) {
		claudeStatus.value = { installed: false, error: (error as Error).message };
	} finally {
		checkingClaude.value = false;
	}
}

function getEffectiveBaseUrl(): string {
	const url = customBaseUrl.value || props.baseUrl;
	return url.endsWith("/v1") ? url : `${url}/v1`;
}

function getDisplayUrl(): string {
	const url = customBaseUrl.value || props.baseUrl;
	return url.endsWith("/v1") ? url : `${url}/v1`;
}

async function handleApplySettings() {
	applying.value = true;
	message.value = null;
	try {
		const env: Record<string, string> = { ANTHROPIC_BASE_URL: getEffectiveBaseUrl() };

		// Get key from dropdown, fallback to first key or sk_9router for localhost
		const keyToUse =
			selectedApiKey.value?.trim() ||
			(props.apiKeys && props.apiKeys.length > 0 ? props.apiKeys[0].key : null) ||
			(!props.cloudEnabled ? "sk_9router" : null);

		if (keyToUse) {
			env.ANTHROPIC_AUTH_TOKEN = keyToUse;
		}

		props.tool.defaultModels?.forEach((model) => {
			const targetModel = props.modelMappings[model.alias];
			// Written verbatim — the input may hold a marker typed by hand, and the
			// toggle already decided the marker when it was flipped.
			if (targetModel && model.envKey) env[model.envKey] = targetModel;
		});
		if (autoCompactWindow.value) {
			env.CLAUDE_CODE_AUTO_COMPACT_WINDOW = autoCompactWindow.value;
		}
		const res = await fetch("/api/cli-tools/claude-settings", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ env, exaMcpEnabled: exaMcpEnabled.value, autoCompactWindow: autoCompactWindow.value }),
		});
		const data = await res.json();
		if (res.ok) {
			// Remember the endpoint so it stays selectable next time
			rememberEndpoint(getEffectiveBaseUrl());
			message.value = { type: "success", text: "Settings applied successfully!" };
			claudeStatus.value = {
				...claudeStatus.value,
				hasBackup: true,
				settings: { ...claudeStatus.value?.settings, env },
				exaMcpEnabled: exaMcpEnabled.value,
			};
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
		const res = await fetch("/api/cli-tools/claude-settings", { method: "DELETE" });
		const data = await res.json();
		if (res.ok) {
			message.value = { type: "success", text: "Settings reset successfully!" };
			props.tool.defaultModels?.forEach((model) => {
				onModelMappingChange(model.alias, model.defaultValue || "");
			});
			selectedApiKey.value = "";
			exaMcpEnabled.value = false;
			autoCompactWindow.value = "";
			oneMContext.value = false;
		} else {
			message.value = { type: "error", text: data.error || "Failed to reset settings" };
		}
	} catch (error) {
		message.value = { type: "error", text: (error as Error).message };
	} finally {
		restoring.value = false;
	}
}

function openModelSelector(alias: string) {
	currentEditingAlias.value = alias;
	modalOpen.value = true;
}

function handleModelSelect(model: { value: string }) {
	if (currentEditingAlias.value) onModelMappingChange(currentEditingAlias.value, model.value);
}

// Generate settings.json content for manual copy
function getManualConfigs() {
	const keyToUse =
		selectedApiKey.value?.trim()
			? selectedApiKey.value
			: !props.cloudEnabled
				? "sk_9router"
				: "<API_KEY_FROM_DASHBOARD>";
	const env: Record<string, string> = {
		ANTHROPIC_BASE_URL: getEffectiveBaseUrl(),
		ANTHROPIC_AUTH_TOKEN: keyToUse,
	};
	props.tool.defaultModels?.forEach((model) => {
		const targetModel = props.modelMappings[model.alias];
		if (targetModel && model.envKey) env[model.envKey] = targetModel;
	});
	if (autoCompactWindow.value) {
		env.CLAUDE_CODE_AUTO_COMPACT_WINDOW = autoCompactWindow.value;
	}

	return [
		{
			filename: "~/.claude/settings.json",
			content: JSON.stringify({ hasCompletedOnboarding: true, env }, null, 2),
		},
	];
}
</script>

<template>
  <Card padding="xs" class="overflow-hidden">
    <button type="button" class="flex w-full items-start justify-between gap-3 text-left hover:cursor-pointer sm:items-center" @click="props.onToggle">
      <div class="flex min-w-0 items-center gap-3">
        <div class="size-8 flex items-center justify-center shrink-0">
          <img
            src="/providers/claude.png"
            :alt="props.tool.name"
            width="32"
            height="32"
            class="size-8 object-contain rounded-lg"
            sizes="32px"
            loading="lazy"
            decoding="async"
            @error="($event.target as HTMLImageElement).style.display = 'none'"
          />
        </div>
        <div class="min-w-0">
          <div class="flex min-w-0 flex-wrap items-center gap-2">
            <h3 class="font-medium text-sm">{{ props.tool.name }}</h3>
            <span v-if="configStatus === 'configured'" class="px-1.5 py-0.5 text-[10px] font-medium bg-green-500/10 text-green-600 dark:text-green-400 rounded-full">Connected</span>
            <span v-if="configStatus === 'not_configured'" class="px-1.5 py-0.5 text-[10px] font-medium bg-yellow-500/10 text-yellow-600 dark:text-yellow-400 rounded-full">Not configured</span>
            <span v-if="configStatus === 'other'" class="px-1.5 py-0.5 text-[10px] font-medium bg-blue-500/10 text-blue-600 dark:text-blue-400 rounded-full">Other</span>
          </div>
          <p class="text-xs text-text-muted truncate">{{ props.tool.description }}</p>
        </div>
      </div>
      <span :class="`material-symbols-outlined text-text-muted text-[20px] transition-transform ${props.isExpanded ? 'rotate-180' : ''}`">expand_more</span>
    </button>

    <div v-if="props.isExpanded" class="mt-4 pt-4 border-t border-border flex flex-col gap-4">
      <div v-if="checkingClaude" class="flex items-center gap-2 text-text-muted">
        <span class="material-symbols-outlined animate-spin">progress_activity</span>
        <span>Checking Claude CLI...</span>
      </div>

      <div v-if="!checkingClaude && claudeStatus && !claudeStatus.installed" class="flex flex-col gap-4">
        <div class="flex flex-col gap-3 p-4 bg-yellow-500/10 border border-yellow-500/30 rounded-lg">
          <div class="flex items-start gap-3">
            <span class="material-symbols-outlined text-yellow-500">warning</span>
            <div class="flex-1">
              <p class="font-medium text-yellow-600 dark:text-yellow-400">Claude CLI not detected locally</p>
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
              <code class="block px-3 py-2 bg-black/5 dark:bg-white/5 rounded font-mono text-xs">npm install -g @anthropic-ai/claude-code</code>
            </div>
            <p class="text-text-muted">After installation, run <code class="px-1 bg-black/5 dark:bg-white/5 rounded">claude</code> to verify.</p>
          </div>
        </div>
      </div>

      <template v-if="!checkingClaude && claudeStatus?.installed">
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
          <div v-if="claudeStatus?.settings?.env?.ANTHROPIC_BASE_URL" class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Current</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <span class="min-w-0 truncate rounded bg-surface/40 px-2 py-2 text-xs text-text-muted sm:py-1.5">
              {{ claudeStatus.settings.env.ANTHROPIC_BASE_URL }}
            </span>
          </div>

          <!-- API Key -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">API Key</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <ApiKeySelect :model-value="selectedApiKey" :api-keys="props.apiKeys" :cloud-enabled="props.cloudEnabled" @update:model-value="selectedApiKey = $event" />
          </div>

          <!-- Model Mappings -->
          <div v-for="model in props.tool.defaultModels" :key="model.alias" class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">{{ model.name }}</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <div class="relative w-full min-w-0">
              <input
                type="text"
                :value="props.modelMappings[model.alias] || ''"
                placeholder="provider/model-id"
                class="w-full min-w-0 pl-2 pr-7 py-2 bg-surface rounded border border-border text-xs focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
                @input="onModelMappingChange(model.alias, ($event.target as HTMLInputElement).value)"
              />
              <button
                v-if="props.modelMappings[model.alias]"
                type="button"
                title="Clear"
                class="absolute right-1 top-1/2 -translate-y-1/2 p-0.5 text-text-muted hover:text-red-500 rounded transition-colors"
                @click="onModelMappingChange(model.alias, '')"
              >
                <span class="material-symbols-outlined text-[14px]">close</span>
              </button>
            </div>
            <button
              type="button"
              :disabled="!props.hasActiveProviders"
              :class="`w-full sm:w-auto rounded border px-2 py-2 text-xs transition-colors sm:py-1.5 whitespace-nowrap sm:shrink-0 ${props.hasActiveProviders ? 'bg-surface border-border text-text-main hover:border-primary cursor-pointer' : 'opacity-50 cursor-not-allowed border-border'}`"
              @click="openModelSelector(model.alias)"
            >
              Select Model
            </button>
          </div>

          <!-- Auto-compact window -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Auto-compact</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <select
              :value="autoCompactWindow"
              class="w-full min-w-0 px-2 py-2 bg-surface rounded border border-border text-xs focus:outline-none focus:ring-1 focus:ring-primary/50 sm:py-1.5"
              @change="autoCompactWindow = ($event.target as HTMLSelectElement).value"
            >
              <option v-for="opt in CONTEXT_OPTIONS" :key="opt.label" :value="opt.value">{{ opt.label }}</option>
            </select>
          </div>

          <!-- 1M context -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">1M context</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <label class="flex items-center gap-1.5 cursor-pointer select-none">
              <input
                type="checkbox"
                :checked="oneMContext"
                class="w-3.5 h-3.5 accent-primary cursor-pointer"
                @change="handleOneMContextToggle(($event.target as HTMLInputElement).checked)"
              />
              <span class="text-xs text-text-muted">Append [1m] to the model name</span>
              <Tooltip text="Claude Code otherwise assumes a 200K window, which clamps the auto-compact window above. Applied to every mapped model — only enable it for models that really accept 1M.">
                <span class="material-symbols-outlined text-text-muted text-[14px] cursor-help">info</span>
              </Tooltip>
            </label>
          </div>

          <!-- CC Filter Naming -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Filter naming</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <label class="flex items-center gap-1.5 cursor-pointer select-none">
              <input type="checkbox" :checked="ccFilterNaming" class="w-3.5 h-3.5 accent-primary cursor-pointer" @change="handleCcFilterNamingToggle" />
              <span class="text-xs text-text-muted">Filter naming requests</span>
              <Tooltip text="Intercepts Claude Code's topic-naming requests and returns a fake response locally, saving API tokens.">
                <span class="material-symbols-outlined text-text-muted text-[14px] cursor-help">info</span>
              </Tooltip>
            </label>
          </div>

          <!-- Exa MCP — ~/.claude.json mcpServers (not settings.json) -->
          <div class="grid grid-cols-1 gap-1.5 sm:grid-cols-[8rem_auto_1fr_auto] sm:items-center sm:gap-2">
            <span class="text-xs font-semibold text-text-main sm:text-right sm:text-sm">Web Search</span>
            <span class="material-symbols-outlined hidden text-text-muted text-[14px] sm:inline">arrow_forward</span>
            <label class="flex items-center gap-1.5 cursor-pointer select-none">
              <input
                type="checkbox"
                :checked="exaMcpEnabled"
                class="w-3.5 h-3.5 accent-primary cursor-pointer"
                @change="exaMcpEnabled = ($event.target as HTMLInputElement).checked"
              />
              <span class="text-xs text-text-muted">Exa MCP</span>
              <Tooltip text="Injects Exa MCP into ~/.claude.json so non-Claude models gain web search. Restart Claude Code after Apply.">
                <span class="material-symbols-outlined text-text-muted text-[14px] cursor-help">info</span>
              </Tooltip>
            </label>
          </div>
        </div>

        <div v-if="message" :class="`flex items-center gap-2 px-2 py-1.5 rounded text-xs ${message.type === 'success' ? 'bg-green-500/10 text-green-600' : 'bg-red-500/10 text-red-600'}`">
          <span class="material-symbols-outlined text-[14px]">{{ message.type === "success" ? "check_circle" : "error" }}</span>
          <span>{{ message.text }}</span>
        </div>

        <div class="grid grid-cols-1 gap-2 sm:flex sm:items-center">
          <Button variant="primary" size="sm" :disabled="!props.hasActiveProviders" :loading="applying" @click="handleApplySettings">
            <span class="material-symbols-outlined text-[14px] mr-1">save</span>Apply
          </Button>
          <Button variant="outline" size="sm" :disabled="!claudeStatus?.has9Router" :loading="restoring" @click="handleResetSettings">
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
      :selected-model="currentEditingAlias ? props.modelMappings[currentEditingAlias] : ''"
      :active-providers="props.activeProviders"
      :model-aliases="modelAliases"
      :title="`Select model for ${currentEditingAlias}`"
      @close="modalOpen = false"
      @select="handleModelSelect"
    />

    <ManualConfigModal
      :is-open="showManualConfigModal"
      title="Claude CLI - Manual Configuration"
      :configs="getManualConfigs()"
      @close="showManualConfigModal = false"
    />
  </Card>
</template>
