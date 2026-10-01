<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { RouterLink, useRoute } from "vue-router";

import CardSkeleton from "@/components/ui/CardSkeleton.vue";
import { CLI_TOOLS } from "@/constants/cliTools";
import { useModels } from "@/constants/models";
import ClaudeToolCard from "./cli-tools/components/ClaudeToolCard.vue";
import CodexToolCard from "./cli-tools/components/CodexToolCard.vue";
import HermesToolCard from "./cli-tools/components/HermesToolCard.vue";

interface Connection {
	provider: string;
	name?: string;
	isActive?: boolean;
	testStatus?: string;
	defaultModel?: string;
	providerSpecificData?: {
		prefix?: string;
		customModels?: Array<{ id?: string; name?: string }>;
	};
}

interface ApiKey {
	key: string;
}

interface ModelOption {
	value: string;
	label: string;
	provider: string;
	alias: string;
	connectionName?: string;
	modelId: string;
}

const route = useRoute();
const { getModelsByProviderId, PROVIDER_ID_TO_ALIAS } = useModels();

const toolId = computed(() => String(route.params.toolId ?? ""));
const tool = computed(() => CLI_TOOLS[toolId.value]);

const connections = ref<Connection[]>([]);
const loading = ref(true);
const modelMappings = ref<Record<string, Record<string, string>>>({});
const apiKeys = ref<ApiKey[]>([]);

onMounted(async () => {
	try {
		const [provRes, keysRes] = await Promise.all([
			fetch("/api/providers"),
			fetch("/api/keys"),
		]);
		if (provRes.ok) {
			const data = await provRes.json();
			connections.value = data.connections || [];
		}
		if (keysRes.ok) {
			const data = await keysRes.json();
			apiKeys.value = data.keys || [];
		}
	} catch (error) {
		console.log("Error loading tool data:", error);
	} finally {
		loading.value = false;
	}
});

const activeProviders = computed(() => connections.value.filter((c) => c.isActive !== false));

function getAllAvailableModels(): ModelOption[] {
	const models: ModelOption[] = [];
	const seenModels = new Set<string>();
	for (const conn of activeProviders.value) {
		const alias = PROVIDER_ID_TO_ALIAS[conn.provider] || conn.provider;
		const providerModels = getModelsByProviderId(conn.provider);
		providerModels.forEach((m) => {
			const modelValue = `${alias}/${m.id}`;
			if (!seenModels.has(modelValue)) {
				seenModels.add(modelValue);
				models.push({
					value: modelValue,
					label: `${alias}/${m.id}`,
					provider: conn.provider,
					alias,
					connectionName: conn.name,
					modelId: m.id,
				});
			}
		});

		// openai/anthropic-compatible providers are registered with a random UUID (e.g.
		// "openai-compatible-chat-<uuid>") that has no entry in the static PROVIDER_MODELS
		// catalog, so `getModelsByProviderId` returns []. Routing still works because the
		// request path uses the connection's own model config, but `hasActiveProviders`
		// below would flip to false and disable the Apply button. Fall back to the
		// connection's own models so these providers are usable from CLI tool pages.
		if (providerModels.length === 0) {
			const prefix = conn.providerSpecificData?.prefix || alias;
			const fallbackModels: Array<{ id: string; name: string }> = [];
			if (conn.defaultModel) fallbackModels.push({ id: conn.defaultModel, name: conn.defaultModel });
			(conn.providerSpecificData?.customModels || []).forEach((m) => {
				if (m?.id && !fallbackModels.some((f) => f.id === m.id))
					fallbackModels.push({ id: m.id, name: m.name || m.id });
			});
			if (fallbackModels.length === 0 && conn.testStatus === "active") {
				// Provider is confirmed reachable but exposes no model info anywhere;
				// still let the user apply so they aren't stuck on a permanently disabled button.
				fallbackModels.push({ id: "model-id", name: `${prefix}/model-id` });
			}
			fallbackModels.forEach((m) => {
				const modelValue = `${prefix}/${m.id}`;
				if (!seenModels.has(modelValue)) {
					seenModels.add(modelValue);
					models.push({
						value: modelValue,
						label: `${prefix}/${m.id}`,
						provider: conn.provider,
						alias: prefix,
						connectionName: conn.name,
						modelId: m.id,
					});
				}
			});
		}
	}
	return models;
}

const availableModels = computed(() => getAllAvailableModels());
const hasActiveProviders = computed(() => availableModels.value.length > 0);

function handleModelMappingChange(tId: string, alias: string, target: string) {
	const current = modelMappings.value[tId]?.[alias];
	if (current === target) return;
	modelMappings.value = {
		...modelMappings.value,
		[tId]: { ...modelMappings.value[tId], [alias]: target },
	};
}

const baseUrl = computed(() => {
	if (typeof window !== "undefined") return window.location.origin;
	return "http://localhost:20129";
});

const commonProps = computed(() => ({
	tool: tool.value,
	isExpanded: true,
	baseUrl: baseUrl.value,
	apiKeys: apiKeys.value,
}));
</script>

<template>
  <div class="mx-auto flex w-full max-w-5xl flex-col gap-4 px-1 sm:px-0">
    <RouterLink
      to="/dashboard/cli-tools"
      class="inline-flex items-center gap-1 text-sm text-text-muted hover:text-primary w-fit"
    >
      <span class="material-symbols-outlined text-[18px]">arrow_back</span>
      Back to CLI Tools
    </RouterLink>

    <template v-if="!tool">
      <p class="text-sm text-text-muted">Tool not found or disabled.</p>
    </template>

    <template v-else>
      <div class="flex flex-col gap-1">
        <h1 class="text-xl font-semibold text-text-main sm:text-2xl">{{ tool.name }}</h1>
        <p class="text-sm text-text-muted">{{ tool.description }}</p>
      </div>
      <CardSkeleton v-if="loading" />
      <ClaudeToolCard
        v-else-if="toolId === 'claude'"
        v-bind="commonProps"
        :active-providers="activeProviders"
        :model-mappings="modelMappings[toolId] || {}"
        :on-model-mapping-change="(a: string, t: string) => handleModelMappingChange(toolId, a, t)"
        :has-active-providers="hasActiveProviders"
      />
      <CodexToolCard
        v-else-if="toolId === 'codex'"
        v-bind="commonProps"
        :active-providers="activeProviders"
      />
      <HermesToolCard
        v-else-if="toolId === 'hermes'"
        v-bind="commonProps"
        :has-active-providers="hasActiveProviders"
        :active-providers="activeProviders"
      />
    </template>
  </div>
</template>
