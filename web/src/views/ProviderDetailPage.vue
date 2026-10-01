<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { RouterLink, useRoute, useRouter } from "vue-router";

import NoAuthProxyCard from "@/components/NoAuthProxyCard.vue";
import OAuthModal from "@/components/OAuthModal.vue";
import CardSkeleton from "@/components/ui/CardSkeleton.vue";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Modal from "@/components/ui/UiModal.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { getModelKind, useModels } from "@/constants/models";
import { isAnthropicCompatibleProvider, isOpenAICompatibleProvider, useProviders } from "@/constants/providers";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";
import { useModelCaps } from "@/hooks/useModelCaps";
import { getProviderCustomModelRows } from "@/utils/providerCustomModels";
import { getProviderIconSrc, markProviderIconMissing } from "@/utils/providerIcon";
import { fetchSuggestedModels } from "@/utils/providerModelsFetcher";
import { getThinkingLevels } from "@/utils/thinkingLevels";
import AddApiKeyModal from "./providers/components/AddApiKeyModal.vue";
import AddCustomModelModal from "./providers/components/AddCustomModelModal.vue";
import BulkImportCodexModal from "./providers/components/BulkImportCodexModal.vue";
import BulkImportGrokCliModal from "./providers/components/BulkImportGrokCliModal.vue";
import CompatibleModelsSection from "./providers/components/CompatibleModelsSection.vue";
import ConnectionRow from "./providers/components/ConnectionRow.vue";
import EditCompatibleNodeModal from "./providers/components/EditCompatibleNodeModal.vue";
import EditConnectionModal from "./providers/components/EditConnectionModal.vue";
import ModelRow from "./providers/components/ModelRow.vue";

const ONE_BY_ONE_DELAY_MS = 1000;

const AUTO_PING_SETTINGS_KEYS: Record<string, string> = {
	codex: "codexAutoPing",
};

function sleep(ms: number) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

const route = useRoute();
const router = useRouter();
const providerId = String(route.params.id || "");

const { getCaps } = useModelCaps();
const { getModelsByProviderId, PROVIDER_MODELS } = useModels();
const {
	OAUTH_PROVIDERS,
	APIKEY_PROVIDERS,
	FREE_PROVIDERS,
	FREE_TIER_PROVIDERS,
	WEB_COOKIE_PROVIDERS,
	getProviderAlias,
} = useProviders();
const { copied, copy } = useCopyToClipboard();

const connections = ref<Array<Record<string, any>>>([]);
const loading = ref(true);
const providerNode = ref<Record<string, any> | null>(null);
const proxyPools = ref<Array<Record<string, any>>>([]);
const showOAuthModal = ref(false);
const showAddApiKeyModal = ref(false);
const addConnectionError = ref("");
const showBulkImportCodex = ref(false);
const showBulkImportGrokCli = ref(false);
const showEditModal = ref(false);
const showEditNodeModal = ref(false);
const showBulkProxyModal = ref(false);
const selectedConnection = ref<Record<string, any> | null>(null);
const modelAliases = ref<Record<string, string>>({});
const customModels = ref<Array<Record<string, any>>>([]);
const headerImgError = ref(false);
const modelTestResults = ref<Record<string, "ok" | "error">>({});
const modelsTestError = ref("");
const testingModelIds = ref<Set<string>>(new Set());
const showAddCustomModel = ref(false);
const selectedConnectionIds = ref<string[]>([]);
const bulkUpdatingProxy = ref(false);
const providerStrategy = ref<string | null>(null);
const providerStickyLimit = ref("");
const thinkingMode = ref("auto");
const autoPing = ref<{ enabled: boolean; connections: Record<string, boolean> }>({
	enabled: false,
	connections: {},
});
const suggestedModels = ref<Array<Record<string, any>>>([]);
const disabledModelIds = ref<string[]>([]);
const confirmState = ref<{
	title: string;
	message: string;
	onConfirm: () => void;
} | null>(null);
const oneByOneRunning = ref(false);
const oneByOneStopping = ref(false);
const oneByOneCurrentConnectionId = ref<string | null>(null);
const oneByOneResults = ref<Record<string, { state: string; error: string | null }>>({});
const oneByOneSummary = ref<{
	total: number;
	completed: number;
	passed: number;
	failed: number;
	stopped: boolean;
} | null>(null);
let stopOneByOne = false;

const providerInfo = computed(() =>
	providerNode.value
		? {
				id: providerNode.value.id,
				name:
					providerNode.value.name ||
					(providerNode.value.type === "anthropic-compatible"
						? "Anthropic Compatible"
						: "OpenAI Compatible"),
				color: providerNode.value.type === "anthropic-compatible" ? "#D97757" : "#10A37F",
				textIcon: providerNode.value.type === "anthropic-compatible" ? "AC" : "OC",
				apiType: providerNode.value.apiType,
				baseUrl: providerNode.value.baseUrl,
				type: providerNode.value.type,
			}
		: OAUTH_PROVIDERS[providerId] ||
			APIKEY_PROVIDERS[providerId] ||
			FREE_PROVIDERS[providerId] ||
			FREE_TIER_PROVIDERS[providerId] ||
			WEB_COOKIE_PROVIDERS[providerId],
);

const authModes = computed(() => providerInfo.value?.authModes || []);
const isOAuth = computed(
	() =>
		!!OAUTH_PROVIDERS[providerId] ||
		!!FREE_PROVIDERS[providerId] ||
		authModes.value.includes("oauth"),
);
const supportsApiKeyAuth = computed(
	() => !!APIKEY_PROVIDERS[providerId] || authModes.value.includes("apikey"),
);
const isFreeNoAuth = computed(() => !!FREE_PROVIDERS[providerId]?.noAuth);
const models = computed(() => getModelsByProviderId(providerId));
const providerAlias = computed(() => getProviderAlias(providerId));
const isOpenAICompatible = computed(() => isOpenAICompatibleProvider(providerId));
const isAnthropicCompatible = computed(() => isAnthropicCompatibleProvider(providerId));
const isCompatible = computed(() => isOpenAICompatible.value || isAnthropicCompatible.value);
const hasDualAuthModes = computed(
	() => !isCompatible.value && isOAuth.value && supportsApiKeyAuth.value,
);
const oauthConnectionLabel = computed(() => providerId === "grok-cli" ? "Grok CLI Device Login" : "OAuth");
const apiKeyConnectionLabel = computed(() => "API Key");
const providerStorageAlias = computed(() => (isCompatible.value ? providerId : providerAlias.value));
const providerDisplayAlias = computed(() =>
	isCompatible.value ? providerNode.value?.prefix || providerId : providerAlias.value,
);
const providerModelRows = computed<Array<Record<string, any>>>(
	() => PROVIDER_MODELS[providerAlias.value] || [],
);

function resolveThinkingSuffix(modelId: string): string | null {
	if (!thinkingMode.value || thinkingMode.value === "auto") return null;
	const levels = getThinkingLevels(
		providerId,
		modelId,
		getCaps(`${providerId}/${modelId}`),
		providerModelRows.value,
	);
	return levels?.includes(thinkingMode.value) ? thinkingMode.value : null;
}

// Union of levels across this provider's reasoning models — drives the picker.
// Include custom models too (e.g. manually added gpt-5.6-sol → max).
const providerThinkingLevels = computed(() => {
	const set = new Set<string>();
	const seen = new Set<string>();
	const addLevels = (modelId: string) => {
		if (!modelId || seen.has(modelId)) return;
		seen.add(modelId);
		const levels = getThinkingLevels(
			providerId,
			modelId,
			getCaps(`${providerId}/${modelId}`),
			providerModelRows.value,
		);
		if (levels) for (const l of levels) if (l !== "none") set.add(l);
	};
	for (const m of models.value) addLevels(m.id);
	for (const entry of customModels.value) {
		if (entry.providerAlias !== providerStorageAlias.value) continue;
		if ((entry.kind || entry.type || "llm") !== "llm") continue;
		addLevels(entry.id);
	}
	return set.size ? ["auto", ...[...set]] : null;
});

const thinkingOptions = computed(() =>
	(providerThinkingLevels.value || []).map((opt) => ({
		value: opt,
		label: `Thinking: ${opt.charAt(0).toUpperCase()}${opt.slice(1)}`,
	})),
);

const activePools = computed(() => proxyPools.value.filter((p) => p.isActive === true));

function openOAuthConnection() {
	showOAuthModal.value = true;
}

function triggerOAuthConnection() {
	if (isOAuth.value) {
		openOAuthConnection();
		return;
	}
	addConnectionError.value = "";
	showAddApiKeyModal.value = true;
}

function triggerApiKeyConnection() {
	addConnectionError.value = "";
	showAddApiKeyModal.value = true;
}

function triggerAddConnection() {
	if (isOAuth.value) {
		triggerOAuthConnection();
		return;
	}
	triggerApiKeyConnection();
}

async function fetchDisabledModels() {
	try {
		const res = await fetch(
			`/api/models/disabled?providerAlias=${encodeURIComponent(providerStorageAlias.value)}`,
			{ cache: "no-store" },
		);
		const data = await res.json();
		if (res.ok) disabledModelIds.value = data.ids || [];
	} catch (error) {
		console.log("Error fetching disabled models:", error);
	}
}

async function handleDisableModel(modelId: string) {
	try {
		const res = await fetch("/api/models/disabled", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ providerAlias: providerStorageAlias.value, ids: [modelId] }),
		});
		if (res.ok) await fetchDisabledModels();
	} catch (error) {
		console.log("Error disabling model:", error);
	}
}

async function handleEnableModel(modelId: string) {
	try {
		const res = await fetch(
			`/api/models/disabled?providerAlias=${encodeURIComponent(providerStorageAlias.value)}&id=${encodeURIComponent(modelId)}`,
			{ method: "DELETE" },
		);
		if (res.ok) await fetchDisabledModels();
	} catch (error) {
		console.log("Error enabling model:", error);
	}
}

function handleDisableAll(ids: string[]) {
	if (!ids.length) return;
	confirmState.value = {
		title: "Disable All Models",
		message: `Disable all ${ids.length} model(s)?`,
		onConfirm: async () => {
			confirmState.value = null;
			try {
				const res = await fetch("/api/models/disabled", {
					method: "POST",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify({ providerAlias: providerStorageAlias.value, ids }),
				});
				if (res.ok) await fetchDisabledModels();
			} catch (error) {
				console.log("Error disabling all models:", error);
			}
		},
	};
}

async function handleEnableAll() {
	try {
		const res = await fetch(
			`/api/models/disabled?providerAlias=${encodeURIComponent(providerStorageAlias.value)}`,
			{ method: "DELETE" },
		);
		if (res.ok) await fetchDisabledModels();
	} catch (error) {
		console.log("Error enabling all models:", error);
	}
}

async function fetchAliases() {
	try {
		const res = await fetch("/api/models/alias");
		const data = await res.json();
		if (res.ok) modelAliases.value = data.aliases || {};
	} catch (error) {
		console.log("Error fetching aliases:", error);
	}
}

async function fetchCustomModels() {
	try {
		const res = await fetch("/api/models/custom", { cache: "no-store" });
		const data = await res.json();
		if (res.ok) customModels.value = data.models || [];
	} catch (error) {
		console.log("Error fetching custom models:", error);
	}
}

async function fetchConnections() {
	try {
		const [connectionsRes, nodesRes, proxyPoolsRes, settingsRes] = await Promise.all([
			fetch("/api/providers", { cache: "no-store" }),
			fetch("/api/provider-nodes", { cache: "no-store" }),
			fetch("/api/proxy-pools?isActive=true", { cache: "no-store" }),
			fetch("/api/settings", { cache: "no-store" }),
		]);
		const connectionsData = await connectionsRes.json();
		const nodesData = await nodesRes.json();
		const proxyPoolsData = await proxyPoolsRes.json();
		const settingsData = settingsRes.ok ? await settingsRes.json() : {};
		if (connectionsRes.ok) {
			connections.value = (connectionsData.connections || []).filter(
				(c: Record<string, any>) => c.provider === providerId,
			);
		}
		if (proxyPoolsRes.ok) {
			proxyPools.value = proxyPoolsData.proxyPools || [];
		}
		// Load per-provider strategy override
		const override = settingsData.providerStrategies?.[providerId] || {};
		providerStrategy.value = override.fallbackStrategy || null;
		providerStickyLimit.value =
			override.stickyRoundRobinLimit != null ? String(override.stickyRoundRobinLimit) : "1";
		// Load per-provider thinking config
		const thinkingCfg = settingsData.providerThinking?.[providerId] || {};
		thinkingMode.value = thinkingCfg.mode || "auto";
		const autoPingSettingsKey = AUTO_PING_SETTINGS_KEYS[providerId];
		const apCfg = autoPingSettingsKey ? settingsData[autoPingSettingsKey] || {} : {};
		autoPing.value = { enabled: apCfg.enabled === true, connections: apCfg.connections || {} };
		if (nodesRes.ok) {
			let node =
				(nodesData.nodes || []).find((entry: Record<string, any>) => entry.id === providerId) ||
				null;

			// Newly created compatible nodes can be briefly unavailable on one worker.
			// Retry a few times before showing "Provider not found".
			if (!node && isCompatible.value) {
				for (let attempt = 0; attempt < 3; attempt += 1) {
					await new Promise((resolve) => setTimeout(resolve, 150));
					const retryRes = await fetch("/api/provider-nodes", { cache: "no-store" });
					if (!retryRes.ok) continue;
					const retryData = await retryRes.json();
					node =
						(retryData.nodes || []).find(
							(entry: Record<string, any>) => entry.id === providerId,
						) || null;
					if (node) break;
				}
			}

			providerNode.value = node;
		}
	} catch (error) {
		console.log("Error fetching connections:", error);
	} finally {
		loading.value = false;
	}
}

async function handleUpdateNode(formData: Record<string, any>) {
	try {
		const res = await fetch(`/api/provider-nodes/${providerId}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(formData),
		});
		const data = await res.json();
		if (res.ok) {
			providerNode.value = data.node;
			await fetchConnections();
			showEditNodeModal.value = false;
		}
	} catch (error) {
		console.log("Error updating provider node:", error);
	}
}

function handleDeleteNode() {
	confirmState.value = {
		title: "Delete Compatible Node",
		message: `Delete this ${isAnthropicCompatible.value ? "Anthropic" : "OpenAI"} Compatible node?`,
		onConfirm: async () => {
			confirmState.value = null;
			try {
				const res = await fetch(`/api/provider-nodes/${providerId}`, { method: "DELETE" });
				if (res.ok) {
					router.push("/dashboard/providers");
				}
			} catch (error) {
				console.log("Error deleting provider node:", error);
			}
		},
	};
}

async function saveProviderStrategy(strategy: string | null, stickyLimit: string) {
	try {
		const settingsRes = await fetch("/api/settings", { cache: "no-store" });
		const settingsData = settingsRes.ok ? await settingsRes.json() : {};
		const current = settingsData.providerStrategies || {};

		// Build override: null strategy means remove override, use global
		const override: Record<string, any> = {};
		if (strategy) override.fallbackStrategy = strategy;
		if (strategy === "round-robin" && stickyLimit !== "") {
			override.stickyRoundRobinLimit = Number(stickyLimit) || 3;
		}

		const updated = { ...current };
		if (Object.keys(override).length === 0) {
			delete updated[providerId];
		} else {
			updated[providerId] = override;
		}

		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ providerStrategies: updated }),
		});
	} catch (error) {
		console.log("Error saving provider strategy:", error);
	}
}

function handleRoundRobinToggle(enabled: boolean) {
	const strategy = enabled ? "round-robin" : null;
	const sticky = enabled ? providerStickyLimit.value || "1" : providerStickyLimit.value;
	if (enabled && !providerStickyLimit.value) providerStickyLimit.value = "1";
	providerStrategy.value = strategy;
	saveProviderStrategy(strategy, sticky);
}

function handleStickyLimitChange(value: string) {
	providerStickyLimit.value = value;
	saveProviderStrategy("round-robin", value);
}

async function saveThinkingConfig(mode: string) {
	try {
		const settingsRes = await fetch("/api/settings", { cache: "no-store" });
		const settingsData = settingsRes.ok ? await settingsRes.json() : {};
		const current = settingsData.providerThinking || {};
		const updated = { ...current };
		if (!mode || mode === "auto") {
			delete updated[providerId];
		} else {
			updated[providerId] = { mode };
		}
		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ providerThinking: updated }),
		});
	} catch (error) {
		console.log("Error saving thinking config:", error);
	}
}

function handleThinkingModeChange(mode: string) {
	thinkingMode.value = mode;
	saveThinkingConfig(mode);
}

async function saveAutoPing(next: { enabled: boolean; connections: Record<string, boolean> }) {
	const autoPingSettingsKey = AUTO_PING_SETTINGS_KEYS[providerId];
	if (!autoPingSettingsKey) return;

	autoPing.value = next;
	try {
		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ [autoPingSettingsKey]: next }),
		});
	} catch (error) {
		console.log("Error saving auto-ping config:", error);
	}
}

function handleAutoPingConnection(connectionId: string, on: boolean) {
	saveAutoPing({
		...autoPing.value,
		connections: { ...autoPing.value.connections, [connectionId]: on },
	});
}

async function handleDeleteAlias(alias: string) {
	try {
		const res = await fetch(`/api/models/alias?alias=${encodeURIComponent(alias)}`, {
			method: "DELETE",
		});
		if (res.ok) {
			await fetchAliases();
		}
	} catch (error) {
		console.log("Error deleting alias:", error);
	}
}

async function handleAddCustomModel(
	modelId: string,
	type = "llm",
	providerAliasOverride: string = providerStorageAlias.value,
	caps?: Record<string, any>,
) {
	try {
		const res = await fetch("/api/models/custom", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				providerAlias: providerAliasOverride,
				id: modelId,
				type,
				...(caps ? { caps } : {}),
			}),
		});
		if (res.ok) {
			await fetchCustomModels();
			window.dispatchEvent(new CustomEvent("customModelChanged"));
		} else {
			const data = await res.json();
			alert(data.error || "Failed to add custom model");
		}
	} catch (error) {
		console.log("Error adding custom model:", error);
	}
}

async function handleDeleteCustomModel(
	modelId: string,
	type = "llm",
	providerAliasOverride: string = providerStorageAlias.value,
) {
	try {
		const params = new URLSearchParams({
			providerAlias: providerAliasOverride,
			id: modelId,
			type,
		});
		const res = await fetch(`/api/models/custom?${params}`, { method: "DELETE" });
		if (res.ok) {
			await fetchCustomModels();
			window.dispatchEvent(new CustomEvent("customModelChanged"));
		}
	} catch (error) {
		console.log("Error deleting custom model:", error);
	}
}

async function handleRunOneByOneTest() {
	if (oneByOneRunning.value || connections.value.length === 0) return;

	const queuedState: Record<string, { state: string; error: string | null }> = {};
	for (const connection of connections.value) {
		queuedState[connection.id] = { state: "queued", error: null };
	}

	stopOneByOne = false;
	oneByOneRunning.value = true;
	oneByOneStopping.value = false;
	oneByOneCurrentConnectionId.value = null;
	oneByOneResults.value = queuedState;
	oneByOneSummary.value = {
		total: connections.value.length,
		completed: 0,
		passed: 0,
		failed: 0,
		stopped: false,
	};

	let passed = 0;
	let failed = 0;

	try {
		for (let index = 0; index < connections.value.length; index += 1) {
			if (stopOneByOne) {
				oneByOneSummary.value = {
					total: connections.value.length,
					completed: index,
					passed,
					failed,
					stopped: true,
				};
				break;
			}

			const connection = connections.value[index];
			oneByOneCurrentConnectionId.value = connection.id;
			oneByOneResults.value = {
				...oneByOneResults.value,
				[connection.id]: { state: "testing", error: null },
			};

			try {
				const res = await fetch(`/api/providers/${connection.id}/test`, { method: "POST" });
				const data = await res.json();
				const valid = !!data.valid;

				if (valid) {
					passed += 1;
				} else {
					failed += 1;
				}

				oneByOneResults.value = {
					...oneByOneResults.value,
					[connection.id]: {
						state: valid ? "success" : "failed",
						error: valid ? null : data.error || null,
					},
				};
			} catch (error) {
				failed += 1;
				oneByOneResults.value = {
					...oneByOneResults.value,
					[connection.id]: {
						state: "failed",
						error: (error as Error).message || "Test failed",
					},
				};
			}

			oneByOneSummary.value = {
				total: connections.value.length,
				completed: index + 1,
				passed,
				failed,
				stopped: false,
			};

			if (index < connections.value.length - 1) {
				await sleep(ONE_BY_ONE_DELAY_MS);
			}
		}
	} finally {
		oneByOneCurrentConnectionId.value = null;
		oneByOneRunning.value = false;
		oneByOneStopping.value = false;
		stopOneByOne = false;
	}
}

function handleStopOneByOneTest() {
	if (!oneByOneRunning.value) return;
	stopOneByOne = true;
	oneByOneStopping.value = true;
}

function handleDelete(id: string) {
	confirmState.value = {
		title: "Delete Connection",
		message: "Delete this connection?",
		onConfirm: async () => {
			confirmState.value = null;
			try {
				const res = await fetch(`/api/providers/${id}`, { method: "DELETE" });
				if (res.ok) {
					connections.value = connections.value.filter((c) => c.id !== id);
				}
			} catch (error) {
				console.log("Error deleting connection:", error);
			}
		},
	};
}

function handleBulkDelete() {
	const count = selectedConnectionIds.value.length;
	if (count === 0) return;
	confirmState.value = {
		title: `Delete ${count} Connection${count > 1 ? "s" : ""}`,
		message: `Delete ${count} connection${count > 1 ? "s" : ""}? This cannot be undone.`,
		onConfirm: async () => {
			confirmState.value = null;
			let failed = 0;
			const idsToDelete = [...selectedConnectionIds.value];
			for (const id of idsToDelete) {
				try {
					const res = await fetch(`/api/providers/${id}`, { method: "DELETE" });
					if (!res.ok) failed += 1;
				} catch (error) {
					console.log("Error deleting connection:", error);
					failed += 1;
				}
			}
			connections.value = connections.value.filter((c) => !idsToDelete.includes(c.id));
			selectedConnectionIds.value = [];
			if (failed > 0)
				alert(`Deleted ${idsToDelete.length - failed} connection(s), ${failed} failed.`);
		},
	};
}

function handleOAuthSuccess() {
	fetchConnections();
	showOAuthModal.value = false;
}

async function handleSaveApiKey(formData: Record<string, any>) {
	addConnectionError.value = "";
	try {
		const res = await fetch("/api/providers", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ provider: providerId, ...formData }),
		});

		let data: Record<string, any> | null = null;
		try {
			data = await res.json();
		} catch {
			data = null;
		}

		if (res.ok) {
			await fetchConnections();
			showAddApiKeyModal.value = false;
			return;
		}

		addConnectionError.value = data?.error || "Failed to save connection";
	} catch (error) {
		console.log("Error saving connection:", error);
		addConnectionError.value = "Failed to save connection";
	}
}

async function handleUpdateConnection(formData: Record<string, any>) {
	try {
		const res = await fetch(`/api/providers/${selectedConnection.value?.id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(formData),
		});
		if (res.ok) {
			await fetchConnections();
			showEditModal.value = false;
		}
	} catch (error) {
		console.log("Error updating connection:", error);
	}
}

async function handleUpdateConnectionStatus(id: string, isActive: boolean) {
	try {
		const res = await fetch(`/api/providers/${id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ isActive }),
		});
		if (res.ok) {
			connections.value = connections.value.map((c) => (c.id === id ? { ...c, isActive } : c));
		}
	} catch (error) {
		console.log("Error updating connection status:", error);
	}
}

async function handleUpdateProxy(connectionId: string, proxyPoolId: string | null) {
	try {
		const res = await fetch(`/api/providers/${connectionId}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ proxyPoolId: proxyPoolId || null }),
		});
		if (res.ok) {
			connections.value = connections.value.map((c) =>
				c.id === connectionId
					? {
							...c,
							providerSpecificData: {
								...c.providerSpecificData,
								proxyPoolId: proxyPoolId || null,
							},
						}
					: c,
			);
		}
	} catch (error) {
		console.log("Error updating proxy:", error);
	}
}

async function handleSwapPriority(index1: number, index2: number) {
	// Optimistic update state
	const newConnections = [...connections.value];
	[newConnections[index1], newConnections[index2]] = [
		newConnections[index2],
		newConnections[index1],
	];
	connections.value = newConnections;

	try {
		await Promise.all([
			fetch(`/api/providers/${newConnections[index1].id}`, {
				method: "PUT",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ priority: index1 }),
			}),
			fetch(`/api/providers/${newConnections[index2].id}`, {
				method: "PUT",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ priority: index2 }),
			}),
		]);
	} catch (error) {
		console.log("Error swapping priority:", error);
		await fetchConnections();
	}
}

const allSelected = computed(
	() =>
		connections.value.length > 0 &&
		selectedConnectionIds.value.length === connections.value.length,
);

function toggleSelectConnection(connectionId: string) {
	selectedConnectionIds.value = selectedConnectionIds.value.includes(connectionId)
		? selectedConnectionIds.value.filter((id) => id !== connectionId)
		: [...selectedConnectionIds.value, connectionId];
}

function toggleSelectAllConnections() {
	if (allSelected.value) {
		selectedConnectionIds.value = [];
		return;
	}
	selectedConnectionIds.value = connections.value.map((conn) => conn.id);
}

function closeBulkProxyModal() {
	if (bulkUpdatingProxy.value) return;
	showBulkProxyModal.value = false;
}

async function applyProxyAssignments(
	assignments: Array<{ connectionId: string; proxyPoolId: string | null }>,
) {
	bulkUpdatingProxy.value = true;
	try {
		let failed = 0;
		for (const { connectionId, proxyPoolId } of assignments) {
			try {
				const res = await fetch(`/api/providers/${connectionId}`, {
					method: "PUT",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify({ proxyPoolId }),
				});
				if (!res.ok) failed += 1;
			} catch (e) {
				console.log("Error applying proxy for", connectionId, e);
				failed += 1;
			}
		}
		if (failed > 0) alert(`Updated with ${failed} failed request(s).`);
		await fetchConnections();
		showBulkProxyModal.value = false;
	} finally {
		bulkUpdatingProxy.value = false;
	}
}

function handleApplySinglePool(proxyPoolId: string | null) {
	const targets = connections.value.map((c) => ({ connectionId: c.id, proxyPoolId }));
	return applyProxyAssignments(targets);
}

function handleApplyOneToOne() {
	if (activePools.value.length === 0) {
		alert("No active proxy pools available.");
		return;
	}
	const targets = connections.value.map((c, i) => ({
		connectionId: c.id,
		proxyPoolId: activePools.value[i % activePools.value.length].id,
	}));
	return applyProxyAssignments(targets);
}

function isSelected(connectionId: string) {
	return selectedConnectionIds.value.includes(connectionId);
}

function openEditModal(connection: Record<string, any>) {
	selectedConnection.value = connection;
	showEditModal.value = true;
}

function openAddApiKeyModal() {
	addConnectionError.value = "";
	showAddApiKeyModal.value = true;
}

function closeAddApiKeyModal() {
	addConnectionError.value = "";
	showAddApiKeyModal.value = false;
}

async function handleTestModel(modelId: string) {
	if (testingModelIds.value.has(modelId)) return;
	testingModelIds.value = new Set(testingModelIds.value).add(modelId);
	try {
		const res = await fetch("/api/models/test", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ model: `${providerStorageAlias.value}/${modelId}` }),
		});
		const data = await res.json();
		modelTestResults.value = { ...modelTestResults.value, [modelId]: data.ok ? "ok" : "error" };
		modelsTestError.value = data.ok ? "" : data.error || "Model not reachable";
	} catch {
		modelTestResults.value = { ...modelTestResults.value, [modelId]: "error" };
		modelsTestError.value = "Network error";
	} finally {
		const next = new Set(testingModelIds.value);
		next.delete(modelId);
		testingModelIds.value = next;
	}
}

const allLlmModels = computed<Array<Record<string, any>>>(() =>
	models.value.filter((m) => {
		const k = getModelKind(m);
		return !k || k === "llm";
	}),
);
const disabledSet = computed(() => new Set(disabledModelIds.value));
const displayModels = computed(() => allLlmModels.value.filter((m) => !disabledSet.value.has(m.id)));
const disabledDisplayModels = computed(() =>
	allLlmModels.value.filter((m) => disabledSet.value.has(m.id)),
);
const customModelRows = computed(() =>
	getProviderCustomModelRows({
		customModels: customModels.value,
		modelAliases: modelAliases.value,
		providerAlias: providerStorageAlias.value,
		builtInModels: models.value as Array<{ id: string }>,
		type: "llm",
	}),
);

const activeModelIds = computed(() =>
	allLlmModels.value.map((m) => m.id).filter((id) => !disabledModelIds.value.includes(id)),
);

const suggestedNotAdded = computed(() => {
	const addedFullModels = new Set([
		...Object.values(modelAliases.value),
		...customModelRows.value.map((model) => model.fullModel),
	]);
	const hardcodedIds = new Set(models.value.map((m) => m.id));
	return suggestedModels.value.filter(
		(m) =>
			!addedFullModels.has(`${providerStorageAlias.value}/${m.id}`) && !hardcodedIds.has(m.id),
	);
});

const chinaLinkUrl = computed(() => {
	const str =
		typeof modelsTestError.value === "string"
			? modelsTestError.value
			: JSON.stringify(modelsTestError.value);
	const linkMatch = str.match(/https:\/\/opencode\.ai\/workspace\/[^\s"')]+/);
	const wrkMatch = str.match(/wrk_[0-9A-Za-z]+/);
	if (linkMatch) return linkMatch[0].endsWith("/go") ? linkMatch[0] : `${linkMatch[0]}/go`;
	if (wrkMatch) return `https://opencode.ai/workspace/${wrkMatch[0]}/go`;
	return "https://opencode.ai";
});
const showChinaLink = computed(() =>
	/RegionError|hosted in China|regionNotAllowed/i.test(String(modelsTestError.value)),
);

function getHeaderIconPath(): string | null {
	if (isOpenAICompatible.value && providerInfo.value?.apiType) {
		return providerInfo.value.apiType === "responses"
			? "/providers/oai-r.png"
			: "/providers/oai-cc.png";
	}
	if (isAnthropicCompatible.value) {
		return "/providers/anthropic-m.png";
	}
	return getProviderIconSrc(providerInfo.value?.id) || "";
}

function existingAliasFor(modelId: string): string | undefined {
	const fullModel = `${providerStorageAlias.value}/${modelId}`;
	const oldFormatModel = `${providerId}/${modelId}`;
	return Object.entries(modelAliases.value).find(
		([, m]) => m === fullModel || m === oldFormatModel,
	)?.[0];
}

function handleDeleteExistingAlias(modelId: string) {
	const alias = existingAliasFor(modelId);
	if (alias) handleDeleteAlias(alias);
}

function onHeaderImgError() {
	if (providerInfo.value) markProviderIconMissing(providerInfo.value.id);
	headerImgError.value = true;
}

async function onAddCustomModelSave(modelId: string, caps?: Record<string, any>) {
	await handleAddCustomModel(modelId, "llm", providerStorageAlias.value, caps);
	showAddCustomModel.value = false;
}

onMounted(() => {
	fetchConnections();
	fetchAliases();
	fetchCustomModels();
	fetchDisabledModels();

	// Fetch suggested models from the provider's public API (if configured).
	const fetcher = (
		OAUTH_PROVIDERS[providerId] ||
		APIKEY_PROVIDERS[providerId] ||
		FREE_PROVIDERS[providerId] ||
		FREE_TIER_PROVIDERS[providerId]
	)?.modelsFetcher;
	if (fetcher) {
		fetchSuggestedModels(fetcher).then((list) => {
			suggestedModels.value = list;
		});
	}
});

// Drop selections whose connection no longer exists.
watch(connections, () => {
	selectedConnectionIds.value = selectedConnectionIds.value.filter((id) =>
		connections.value.some((conn) => conn.id === id),
	);
});
</script>

<template>
  <div v-if="loading" class="flex flex-col gap-8">
    <CardSkeleton />
    <CardSkeleton />
  </div>

  <div v-else-if="!providerInfo" class="text-center py-20">
    <p class="text-text-muted">Provider not found</p>
    <RouterLink to="/dashboard/providers" class="text-primary mt-4 inline-block">
      Back to Providers
    </RouterLink>
  </div>

  <div v-else class="flex min-w-0 flex-col gap-6 px-1 sm:gap-8 sm:px-0">
    <!-- Header -->
    <div class="min-w-0">
      <RouterLink
        to="/dashboard/providers"
        class="inline-flex items-center gap-1 text-sm text-text-muted hover:text-primary transition-colors mb-4"
      >
        <span class="material-symbols-outlined text-lg">arrow_back</span>
        Back to Providers
      </RouterLink>
      <div class="flex min-w-0 items-center gap-3 sm:gap-4">
        <div
          class="flex size-12 shrink-0 items-center justify-center rounded-lg"
          :style="{ backgroundColor: `${providerInfo.color}15` }"
        >
          <span
            v-if="headerImgError || !getHeaderIconPath()"
            class="text-sm font-bold"
            :style="{ color: providerInfo.color }"
          >{{ providerInfo.textIcon || providerInfo.id.slice(0, 2).toUpperCase() }}</span>
          <img
            v-else
            :src="getHeaderIconPath() || undefined"
            :alt="providerInfo.name"
            width="48"
            height="48"
            class="max-h-12 max-w-12 rounded-lg object-contain"
            loading="lazy"
            decoding="async"
            @error="onHeaderImgError"
          />
        </div>
        <div class="min-w-0">
          <div class="flex items-center gap-3 flex-wrap">
            <h1 class="truncate text-2xl font-semibold tracking-tight sm:text-3xl">{{ providerInfo.name }}</h1>
            <a
              v-if="providerInfo.notice?.apiKeyUrl || providerInfo.notice?.signupUrl || providerInfo.website"
              :href="providerInfo.notice?.apiKeyUrl || providerInfo.notice?.signupUrl || providerInfo.website"
              target="_blank"
              rel="noopener noreferrer"
              class="text-xs text-primary hover:underline inline-flex items-center gap-1"
            >
              <span class="material-symbols-outlined text-sm">open_in_new</span>
              {{ providerInfo.notice?.apiKeyUrl ? "Get API Key" : "Sign up / Learn more" }}
            </a>
          </div>
          <p class="text-text-muted">
            {{ connections.length }} connection{{ connections.length === 1 ? "" : "s" }}
          </p>
        </div>
      </div>
    </div>

    <div
      v-if="providerInfo.deprecated"
      class="flex items-center gap-2 px-3 py-2 rounded-lg bg-yellow-500/10 border border-yellow-500/30"
    >
      <span class="material-symbols-outlined text-[16px] text-yellow-500 mt-0.5 shrink-0">warning</span>
      <p class="text-xs text-red-600 dark:text-yellow-400 leading-relaxed">{{ providerInfo.deprecationNotice }}</p>
    </div>

    <div
      v-if="providerInfo.notice?.text && !providerInfo.deprecated"
      class="flex flex-col gap-2 rounded-lg border border-blue-500/30 bg-blue-500/10 px-3 py-2 sm:flex-row sm:items-center"
    >
      <span class="material-symbols-outlined text-[16px] text-blue-500 shrink-0">info</span>
      <p class="min-w-0 flex-1 text-xs leading-relaxed text-blue-600 dark:text-blue-400">{{ providerInfo.notice.text }}</p>
      <a
        v-if="providerInfo.notice.apiKeyUrl"
        :href="providerInfo.notice.apiKeyUrl"
        target="_blank"
        rel="noopener noreferrer"
        class="inline-flex justify-center rounded bg-blue-500 px-2 py-1 text-xs font-medium text-white transition-colors hover:bg-blue-600 sm:py-0.5"
      >
        Get API Key →
      </a>
    </div>

    <Card v-if="isCompatible && providerNode">
      <div class="mb-4 flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div class="min-w-0">
          <h2 class="text-lg font-semibold">{{ isAnthropicCompatible ? "Anthropic Compatible Details" : "OpenAI Compatible Details" }}</h2>
          <p class="break-all text-sm text-text-muted">
            {{ isAnthropicCompatible ? "Messages API" : (providerNode.apiType === "responses" ? "Responses API" : "Chat Completions") }} · {{ (providerNode.baseUrl || "").replace(/\/$/, "") }}/{{ isAnthropicCompatible ? "messages" : (providerNode.apiType === "responses" ? "responses" : "chat/completions") }}
          </p>
        </div>
        <div class="grid grid-cols-1 gap-2 sm:flex sm:items-center">
          <Button size="sm" icon="add" class="w-full sm:w-auto" @click="openAddApiKeyModal">
            Add API Key
          </Button>
          <Button
            size="sm"
            variant="secondary"
            icon="edit"
            class="w-full sm:w-auto"
            @click="showEditNodeModal = true"
          >
            Edit
          </Button>
          <Button
            size="sm"
            variant="secondary"
            icon="delete"
            class="w-full sm:w-auto"
            @click="handleDeleteNode"
          >
            Delete
          </Button>
        </div>
      </div>
    </Card>

    <!-- Connections -->
    <NoAuthProxyCard v-if="isFreeNoAuth" :provider-id="providerId" />
    <Card v-else>
      <div class="mb-4 flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <h2 class="text-lg font-semibold">Connections</h2>
        <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:gap-4">
          <Button
            v-if="connections.length > 0 && proxyPools.length > 0"
            size="sm"
            variant="secondary"
            icon="lan"
            @click="showBulkProxyModal = true"
          >
            Apply Proxy
          </Button>
          <template v-if="connections.length > 0">
            <Button
              v-if="selectedConnectionIds.length > 0"
              size="sm"
              variant="danger"
              icon="delete"
              @click="handleBulkDelete"
            >
              Delete Selected ({{ selectedConnectionIds.length }})
            </Button>
            <Button
              size="sm"
              variant="secondary"
              icon="sync"
              :disabled="oneByOneRunning"
              @click="handleRunOneByOneTest"
            >
              {{ oneByOneRunning ? "Testing Connection One-by-One..." : "Test Connection One-by-One" }}
            </Button>
            <Button
              v-if="oneByOneRunning"
              size="sm"
              variant="ghost"
              icon="stop"
              :disabled="oneByOneStopping"
              @click="handleStopOneByOneTest"
            >
              {{ oneByOneStopping ? "Stopping..." : "Stop" }}
            </Button>
          </template>
          <!-- Round Robin toggle -->
          <div class="flex flex-wrap items-center gap-2">
            <span class="text-xs text-text-muted font-medium">Round Robin</span>
            <Toggle
              :model-value="providerStrategy === 'round-robin'"
              @update:model-value="handleRoundRobinToggle"
            />
            <div v-if="providerStrategy === 'round-robin'" class="flex items-center gap-1.5">
              <span class="text-xs text-text-muted">Sticky:</span>
              <input
                type="number"
                min="1"
                :value="providerStickyLimit"
                placeholder="1"
                class="w-14 px-2 py-1 text-xs border border-border rounded-md bg-surface focus:outline-none focus:border-primary"
                @change="handleStickyLimitChange(($event.target as HTMLInputElement).value)"
              />
            </div>
          </div>
        </div>
      </div>

      <div
        v-if="connections.length === 0"
        class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between"
      >
        <div class="flex items-center gap-3">
          <div class="inline-flex items-center justify-center w-9 h-9 rounded-full bg-primary/10 text-primary shrink-0">
            <span class="material-symbols-outlined text-[18px]">{{ isOAuth ? "lock" : "key" }}</span>
          </div>
          <div class="min-w-0">
            <p class="text-sm text-text-muted">No connections yet</p>
            <p v-if="hasDualAuthModes" class="text-xs text-text-muted">
              Choose {{ oauthConnectionLabel }} or {{ apiKeyConnectionLabel }}.
            </p>
          </div>
        </div>
        <div class="flex gap-2">
          <template v-if="hasDualAuthModes">
            <Button size="sm" icon="lock" variant="secondary" @click="triggerOAuthConnection">
              {{ oauthConnectionLabel }}
            </Button>
            <Button size="sm" icon="key" @click="triggerApiKeyConnection">
              {{ apiKeyConnectionLabel }}
            </Button>
          </template>
          <template v-else>
            <Button
              v-if="providerId === 'codex'"
              size="sm"
              icon="playlist_add"
              variant="secondary"
              @click="showBulkImportCodex = true"
            >
              Bulk Add
            </Button>
            <Button
              v-if="providerId === 'grok-cli'"
              size="sm"
              icon="playlist_add"
              variant="secondary"
              @click="showBulkImportGrokCli = true"
            >
              Bulk Add
            </Button>
            <Button size="sm" icon="add" @click="triggerAddConnection">
              {{ isCompatible ? "Add API Key" : "Add Connection" }}
            </Button>
          </template>
        </div>
      </div>

      <template v-else>
        <div
          v-if="oneByOneSummary"
          class="mb-4 rounded-lg border border-black/10 bg-black/2 px-3 py-2 text-xs text-text-muted dark:border-white/10 dark:bg-white/3"
        >
          <div class="flex flex-wrap items-center gap-3">
            <span>Total: {{ oneByOneSummary.total }}</span>
            <span>Completed: {{ oneByOneSummary.completed }}</span>
            <span>Passed: {{ oneByOneSummary.passed }}</span>
            <span>Failed: {{ oneByOneSummary.failed }}</span>
            <span v-if="oneByOneSummary.stopped" class="text-amber-600 dark:text-amber-400">Stopped</span>
            <span v-if="oneByOneRunning && oneByOneCurrentConnectionId">
              Running: {{ connections.find((conn) => conn.id === oneByOneCurrentConnectionId)?.name || oneByOneCurrentConnectionId }}
            </span>
          </div>
        </div>
        <div
          v-if="connections.length > 0"
          class="mb-3 flex items-center gap-2 border-b border-black/3 pb-2 dark:border-white/3"
        >
          <label class="flex cursor-pointer items-center gap-1.5 text-xs text-text-muted hover:text-primary">
            <input
              type="checkbox"
              :checked="allSelected"
              class="h-3.5 w-3.5 rounded border-gray-300 text-primary focus:ring-primary"
              @change="toggleSelectAllConnections"
            />
            Select All
          </label>
        </div>

        <div class="flex min-w-0 flex-col divide-y divide-black/3 dark:divide-white/3 max-h-125 overflow-y-auto pr-1">
          <div v-for="(conn, index) in connections" :key="conn.id" class="flex min-w-0 items-stretch">
            <div class="flex shrink-0 items-center pl-1 sm:pl-2">
              <input
                type="checkbox"
                :checked="isSelected(conn.id)"
                class="h-4 w-4 rounded border-gray-300 text-primary focus:ring-primary"
                @change="toggleSelectConnection(conn.id)"
              />
            </div>
            <div class="flex-1 min-w-0">
              <ConnectionRow
                :connection="conn"
                :proxy-pools="proxyPools"
                :is-o-auth="isOAuth"
                :is-first="index === 0"
                :is-last="index === connections.length - 1"
                :one-by-one-status="oneByOneResults[conn.id] || null"
                :auto-ping="AUTO_PING_SETTINGS_KEYS[providerId] && conn.authType === 'oauth' ? {
                  on: autoPing.connections[conn.id] === true,
                  onToggle: (on: boolean) => handleAutoPingConnection(conn.id, on),
                  provider: providerId,
                } : null"
                @move-up="handleSwapPriority(index, index - 1)"
                @move-down="handleSwapPriority(index, index + 1)"
                @toggle-active="(isActive: boolean) => handleUpdateConnectionStatus(conn.id, isActive)"
                @update-proxy="(proxyPoolId: string | null) => handleUpdateProxy(conn.id, proxyPoolId)"
                @edit="openEditModal(conn)"
                @delete="handleDelete(conn.id)"
              />
            </div>
          </div>
        </div>

        <div v-if="!isCompatible" class="mt-4 grid grid-cols-1 gap-2 sm:flex">
          <Button
            v-if="providerId === 'codex'"
            size="sm"
            icon="playlist_add"
            variant="secondary"
            title="Bulk import codex accounts from JSON"
            class="w-full sm:w-auto"
            @click="showBulkImportCodex = true"
          >
            Bulk Add
          </Button>
          <Button
            v-if="providerId === 'grok-cli'"
            size="sm"
            icon="playlist_add"
            variant="secondary"
            title="Bulk import Grok CLI accounts from JSON"
            class="w-full sm:w-auto"
            @click="showBulkImportGrokCli = true"
          >
            Bulk Add
          </Button>
          <template v-if="hasDualAuthModes">
            <Button
              size="sm"
              icon="lock"
              variant="secondary"
              class="w-full sm:w-auto"
              @click="triggerOAuthConnection"
            >
              {{ oauthConnectionLabel }}
            </Button>
            <Button size="sm" icon="key" class="w-full sm:w-auto" @click="triggerApiKeyConnection">
              {{ apiKeyConnectionLabel }}
            </Button>
          </template>
          <Button v-else size="sm" icon="add" class="w-full sm:w-auto" @click="triggerAddConnection">
            Add
          </Button>
        </div>
      </template>
    </Card>

    <!-- Models -->
    <Card>
      <div class="mb-4 flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
        <div class="flex items-center gap-3">
          <h2 class="text-lg font-semibold">Available Models</h2>
          <select
            v-if="providerThinkingLevels"
            :value="thinkingMode"
            title="Appends (level) suffix to copied model names"
            class="rounded-md border border-border bg-surface px-2 py-1 text-xs focus:border-primary focus:outline-none"
            @change="handleThinkingModeChange(($event.target as HTMLSelectElement).value)"
          >
            <option v-for="opt in thinkingOptions" :key="opt.value" :value="opt.value">{{ opt.label }}</option>
          </select>
        </div>
        <div v-if="!isCompatible" class="flex gap-2">
          <Button
            v-if="disabledModelIds.length > 0"
            size="sm"
            variant="secondary"
            icon="restart_alt"
            @click="handleEnableAll"
          >
            Active All
          </Button>
          <Button
            v-if="activeModelIds.length > 0"
            size="sm"
            variant="secondary"
            icon="block"
            @click="handleDisableAll(activeModelIds)"
          >
            Disable All
          </Button>
        </div>
      </div>
      <div v-if="modelsTestError" class="mb-3">
        <p class="text-xs text-red-500 wrap-break-word">{{ modelsTestError }}</p>
        <div v-if="showChinaLink" class="mt-1.5">
          <a
            :href="chinaLinkUrl"
            target="_blank"
            rel="noreferrer"
            class="inline-flex items-center gap-1 rounded-md bg-amber-500/10 px-2 py-0.5 text-xs font-medium text-amber-600 hover:bg-amber-500/20 dark:text-amber-400 transition-colors"
          >
            <span>Allow China-hosted models</span>
            <span class="material-symbols-outlined text-[13px]">open_in_new</span>
          </a>
        </div>
      </div>

      <CompatibleModelsSection
        v-if="isCompatible"
        :provider-storage-alias="providerStorageAlias"
        :provider-display-alias="providerDisplayAlias"
        :model-aliases="modelAliases"
        :custom-models="customModels"
        :copied="copied"
        :connections="connections"
        :is-anthropic="isAnthropicCompatible"
        @copy="copy"
        @delete-alias="handleDeleteAlias"
        @add-custom-model="(modelId: string) => handleAddCustomModel(modelId, 'llm', providerStorageAlias)"
        @delete-custom-model="(modelId: string) => handleDeleteCustomModel(modelId, 'llm', providerStorageAlias)"
      />

      <div v-else class="flex flex-wrap gap-3">
        <!-- Custom models first -->
        <ModelRow
          v-for="model in customModelRows"
          :key="`${model.source}-${model.fullModel}`"
          :model="{ id: model.id, name: model.name }"
          :full-model="`${providerDisplayAlias}/${model.id}`"
          :alias="model.alias"
          :copied="copied"
          :test-status="modelTestResults[model.id]"
          :is-testing="testingModelIds.has(model.id)"
          :caps="getCaps(`${providerId}/${model.id}`)"
          :thinking-suffix="resolveThinkingSuffix(model.id)"
          is-custom
          :is-free="false"
          @copy="copy"
          @delete-alias="model.source === 'custom' ? handleDeleteCustomModel(model.id, 'llm', providerStorageAlias) : handleDeleteAlias(model.alias)"
          @test="() => handleTestModel(model.id)"
        />

        <ModelRow
          v-for="model in displayModels"
          :key="model.id"
          :model="model"
          :full-model="`${providerDisplayAlias}/${model.id}`"
          :alias="existingAliasFor(model.id)"
          :copied="copied"
          :test-status="modelTestResults[model.id]"
          :is-testing="testingModelIds.has(model.id)"
          :is-free="model.isFree"
          :has-disable="true"
          :caps="getCaps(`${providerId}/${model.id}`)"
          :thinking-suffix="resolveThinkingSuffix(model.id)"
          @copy="copy"
          @delete-alias="handleDeleteExistingAlias(model.id)"
          @test="() => handleTestModel(model.id)"
          @disable="() => handleDisableModel(model.id)"
        />

        <!-- Add model button — inline, same style as model chips -->
        <button
          type="button"
          class="flex w-full items-center justify-center gap-1.5 rounded-lg border border-dashed border-primary/40 px-3 py-2 text-xs text-primary transition-colors hover:border-primary hover:bg-primary/5 sm:w-auto"
          @click="showAddCustomModel = true"
        >
          <span class="material-symbols-outlined text-sm">add</span>
          Add Model
        </button>

        <!-- Suggested models from provider API — show only models not yet added -->
        <div v-if="suggestedNotAdded.length > 0" class="w-full mt-2">
          <p class="text-xs text-text-muted mb-2">Suggested free models (≥200k context):</p>
          <div class="flex flex-wrap gap-2">
            <button
              v-for="m in suggestedNotAdded"
              :key="m.id"
              type="button"
              class="flex items-center gap-1 px-2.5 py-1.5 rounded-lg border border-black/10 dark:border-white/10 text-xs text-text-muted hover:text-primary hover:border-primary/40 hover:bg-primary/5 transition-colors"
              :title="`${m.name} · ${(m.contextLength / 1000).toFixed(0)}k ctx`"
              @click="handleAddCustomModel(m.id, 'llm', providerStorageAlias)"
            >
              <span class="material-symbols-outlined text-[13px]">add</span>
              {{ m.id.split("/").pop() }}
            </button>
          </div>
        </div>

        <!-- Disabled models — restorable -->
        <div v-if="disabledDisplayModels.length > 0" class="w-full mt-2">
          <p class="text-xs text-text-muted mb-2">Disabled models ({{ disabledDisplayModels.length }}):</p>
          <div class="flex flex-wrap gap-2">
            <button
              v-for="m in disabledDisplayModels"
              :key="m.id"
              type="button"
              class="flex items-center gap-1 px-2.5 py-1.5 rounded-lg border border-dashed border-black/10 dark:border-white/10 text-xs text-text-muted hover:text-primary hover:border-primary/40 hover:bg-primary/5 transition-colors"
              title="Restore model"
              @click="handleEnableModel(m.id)"
            >
              <span class="material-symbols-outlined text-[13px]">add</span>
              {{ m.id }}
            </button>
          </div>
        </div>
      </div>
    </Card>

    <!-- Bulk proxy assignment modal -->
    <Modal
      :is-open="showBulkProxyModal"
      :title="`Apply Proxy (${connections.length} connections)`"
      @close="closeBulkProxyModal"
    >
      <div class="flex flex-col gap-3">
        <div class="flex flex-col">
          <button
            type="button"
            :disabled="bulkUpdatingProxy || activePools.length === 0"
            class="flex items-center gap-2 rounded-lg px-3 py-2 text-left transition-colors hover:bg-black/4 dark:hover:bg-white/4 disabled:cursor-not-allowed disabled:opacity-50"
            @click="handleApplyOneToOne"
          >
            <span class="material-symbols-outlined text-text-muted text-[18px]">sync_alt</span>
            <span class="text-sm text-text-main">One-to-one (rotate)</span>
          </button>
          <button
            type="button"
            :disabled="bulkUpdatingProxy"
            class="flex items-center gap-2 rounded-lg px-3 py-2 text-left transition-colors hover:bg-black/4 dark:hover:bg-white/4 disabled:cursor-not-allowed disabled:opacity-50"
            @click="handleApplySinglePool(null)"
          >
            <span class="material-symbols-outlined text-text-muted text-[18px]">link_off</span>
            <span class="text-sm text-text-main">None (unbind all)</span>
          </button>
          <button
            v-for="pool in proxyPools"
            :key="pool.id"
            type="button"
            :disabled="bulkUpdatingProxy || pool.isActive !== true"
            class="flex items-center gap-2 rounded-lg px-3 py-2 text-left transition-colors hover:bg-black/4 dark:hover:bg-white/4 disabled:cursor-not-allowed disabled:opacity-50"
            @click="handleApplySinglePool(pool.id)"
          >
            <span class="material-symbols-outlined text-text-muted text-[18px]">lan</span>
            <span class="truncate text-sm text-text-main">{{ pool.name }}</span>
            <span v-if="pool.isActive !== true" class="text-[10px] text-text-muted">(inactive)</span>
          </button>
        </div>

        <p v-if="bulkUpdatingProxy" class="text-xs text-text-muted">Applying...</p>

        <Button variant="ghost" full-width :disabled="bulkUpdatingProxy" @click="closeBulkProxyModal">
          Cancel
        </Button>
      </div>
    </Modal>

    <!-- Modals -->
    <OAuthModal
      :is-open="showOAuthModal"
      :provider="providerId"
      :provider-info="providerInfo"
      @success="handleOAuthSuccess"
      @close="showOAuthModal = false"
    />

    <AddApiKeyModal
      :is-open="showAddApiKeyModal"
      :provider="providerId"
      :provider-name="providerInfo.name"
      :is-compatible="isCompatible"
      :is-anthropic="isAnthropicCompatible"
      :auth-type="providerInfo?.authType"
      :auth-hint="providerInfo?.authHint"
      :website="providerInfo?.website"
      :proxy-pools="proxyPools"
      :error="addConnectionError"
      :existing-names="connections.map((c) => c.name).filter(Boolean)"
      @save="handleSaveApiKey"
      @bulk-done="fetchConnections"
      @close="closeAddApiKeyModal"
    />
    <EditConnectionModal
      :is-open="showEditModal"
      :connection="selectedConnection"
      :proxy-pools="proxyPools"
      @save="handleUpdateConnection"
      @close="showEditModal = false"
    />
    <EditCompatibleNodeModal
      v-if="isCompatible"
      :is-open="showEditNodeModal"
      :node="providerNode"
      :is-anthropic="isAnthropicCompatible"
      @save="handleUpdateNode"
      @close="showEditNodeModal = false"
    />
    <AddCustomModelModal
      v-if="!isCompatible"
      :is-open="showAddCustomModel"
      :provider-alias="providerStorageAlias"
      :provider-display-alias="providerDisplayAlias"
      @save="onAddCustomModelSave"
      @close="showAddCustomModel = false"
    />

    <BulkImportCodexModal
      v-if="providerId === 'codex'"
      :is-open="showBulkImportCodex"
      @close="showBulkImportCodex = false"
      @success="fetchConnections"
    />

    <BulkImportGrokCliModal
      v-if="providerId === 'grok-cli'"
      :is-open="showBulkImportGrokCli"
      @close="showBulkImportGrokCli = false"
      @success="fetchConnections"
    />

    <!-- Confirm Modal -->
    <ConfirmModal
      :is-open="!!confirmState"
      :title="confirmState?.title || 'Confirm'"
      :message="confirmState?.message"
      variant="danger"
      @close="confirmState = null"
      @confirm="confirmState?.onConfirm()"
    />
  </div>
</template>
