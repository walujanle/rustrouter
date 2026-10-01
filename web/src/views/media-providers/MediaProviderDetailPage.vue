<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { RouterLink, useRouter } from "vue-router";

import AddCustomEmbeddingModal from "@/components/AddCustomEmbeddingModal.vue";
import NoAuthProxyCard from "@/components/NoAuthProxyCard.vue";
import ProviderInfoCard from "@/components/ProviderInfoCard.vue";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import { isCustomEmbeddingProvider, useProviders } from "@/constants/providers";
import ConnectionsCard from "@/views/providers/components/ConnectionsCard.vue";
import ModelsCard from "@/views/providers/components/ModelsCard.vue";
import EmbeddingExampleCard from "./components/EmbeddingExampleCard.vue";
import { KIND_EXAMPLE_CONFIG } from "./components/exampleShared";
import GenericExampleCard from "./components/GenericExampleCard.vue";

// Only embedding (dedicated card) and webSearch/webFetch/systemone (generic
// card) render an example; the TTS/STT branches are gone with those modalities
// and imageToText is not a kind.

const props = defineProps<{ kind: string; id: string }>();

const router = useRouter();
const { AI_PROVIDERS, MEDIA_PROVIDER_KINDS } = useProviders();

const customNode = ref<Record<string, any> | null>(null);
const customLoading = ref(isCustomEmbeddingProvider(props.id) && props.kind === "embedding");
const showEditModal = ref(false);

const kindConfig = computed(() => MEDIA_PROVIDER_KINDS.find((k) => k.id === props.kind));
const isCustom = computed(() => isCustomEmbeddingProvider(props.id) && props.kind === "embedding");

const builtInProvider = computed(() => AI_PROVIDERS[props.id]);
const provider = computed<Record<string, any> | null>(() => {
	if (isCustom.value) {
		return customNode.value
			? { id: props.id, name: customNode.value.name || "Custom Embedding", color: "#6366F1", textIcon: "CE" }
			: null;
	}
	return builtInProvider.value ?? null;
});

const kinds = computed(() => (isCustom.value ? ["embedding"] : (provider.value?.serviceKinds ?? ["llm"])));

// A built-in provider that does not declare this kind, or an unknown id, is a
// dead route.
const notFound = computed(() => {
	if (!kindConfig.value) return true;
	if (isCustom.value) return !customLoading.value && !customNode.value;
	if (!builtInProvider.value) return true;
	return !kinds.value.includes(props.kind);
});

// A webSearch provider with no `searchConfig` gets no card: the generator
// strips the synthetic config built from `searchViaChat`.
const configForKind = computed(() => {
	const p = provider.value;
	if (!p || isCustom.value) return null;
	switch (props.kind) {
		case "webFetch":
			return p.fetchConfig ?? null;
		case "webSearch":
			return p.searchConfig ?? null;
		case "embedding":
			return p.embeddingConfig ?? null;
		case "systemone":
			return p.systemoneConfig ?? null;
		default:
			return null;
	}
});

// Models are hidden for the provider-as-model kinds.
const showModels = computed(
	() => props.kind !== "webSearch" && props.kind !== "webFetch",
);

async function fetchCustomNode() {
	if (!isCustom.value) return;
	try {
		const res = await fetch("/api/provider-nodes", { cache: "no-store" });
		const data = await res.json();
		customNode.value = (data.nodes || []).find((n: Record<string, any>) => n.id === props.id) || null;
	} catch {
		customNode.value = null;
	} finally {
		customLoading.value = false;
	}
}

onMounted(fetchCustomNode);

async function handleDeleteCustom() {
	if (!confirm("Delete this Custom Embedding node?")) return;
	try {
		const res = await fetch(`/api/provider-nodes/${props.id}`, { method: "DELETE" });
		if (res.ok) router.push(`/dashboard/media-providers/${props.kind}`);
	} catch (error) {
		console.log("Error deleting custom embedding node:", error);
	}
}

function onSaved(node: Record<string, any>) {
	customNode.value = node;
	showEditModal.value = false;
}
</script>

<template>
  <div v-if="customLoading" class="text-text-muted text-sm py-12 text-center">Loading...</div>
  <div v-else-if="notFound || !provider" class="text-text-muted text-sm py-12 text-center">
    Provider not found for this media kind.
  </div>

  <div v-else class="flex flex-col gap-8">
    <div>
      <RouterLink
        :to="`/dashboard/media-providers/${kind}`"
        class="inline-flex items-center gap-1 text-sm text-text-muted hover:text-primary transition-colors mb-4"
      >
        <span class="material-symbols-outlined text-lg">arrow_back</span>
        {{ kindConfig?.label }}
      </RouterLink>

      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:gap-4">
        <div
          class="size-12 rounded-lg flex items-center justify-center shrink-0"
          :style="{ backgroundColor: `${provider.color}15` }"
        >
          <ProviderIcon
            :src="`/providers/${provider.id}.png`"
            :alt="provider.name"
            :size="48"
            class="object-contain rounded-lg max-w-[48px] max-h-[48px]"
            :fallback-text="provider.textIcon || provider.id.slice(0, 2).toUpperCase()"
            :fallback-color="provider.color"
          />
        </div>
        <div class="flex-1">
          <div class="flex flex-wrap items-center gap-2 sm:gap-3">
            <h1 class="text-3xl font-semibold tracking-tight">{{ provider.name }}</h1>
            <a
              v-if="!isCustom && provider.notice?.apiKeyUrl"
              :href="provider.notice.apiKeyUrl"
              target="_blank"
              rel="noopener noreferrer"
              class="text-xs text-primary hover:underline inline-flex items-center gap-1"
            >
              <span class="material-symbols-outlined text-sm">open_in_new</span>
              Get API Key
            </a>
          </div>
          <div class="flex items-center gap-1.5 mt-1 flex-wrap">
            <Badge v-if="isCustom" variant="default" size="sm">Custom · {{ customNode?.prefix }}</Badge>
            <Badge v-for="k in kinds" :key="k" :variant="k === kind ? 'primary' : 'default'" size="sm">
              {{ k.toUpperCase() }}
            </Badge>
          </div>
        </div>
        <div v-if="isCustom" class="flex w-full flex-col gap-2 sm:w-auto sm:flex-row sm:items-center">
          <Button size="sm" variant="secondary" icon="edit" @click="showEditModal = true">Edit</Button>
          <Button size="sm" variant="secondary" icon="delete" @click="handleDeleteCustom">Delete</Button>
        </div>
      </div>
    </div>

    <div
      v-if="!isCustom && provider.kindNotice?.[kind]"
      class="flex items-start gap-3 px-4 py-3 rounded-lg bg-amber-500/10 border border-amber-500/30 text-amber-700 dark:text-amber-400"
    >
      <span class="material-symbols-outlined text-[20px] mt-0.5">warning</span>
      <p class="text-sm">{{ provider.kindNotice[kind] }}</p>
    </div>

    <div
      v-if="!isCustom && provider.notice?.text && !provider.deprecated"
      class="flex flex-col gap-2 rounded-lg border border-blue-500/30 bg-blue-500/10 px-3 py-2 sm:flex-row sm:items-center"
    >
      <span class="material-symbols-outlined text-[16px] text-blue-500 shrink-0">info</span>
      <p class="min-w-0 flex-1 text-xs leading-relaxed text-blue-600 dark:text-blue-400">{{ provider.notice.text }}</p>
      <a
        v-if="provider.notice.apiKeyUrl"
        :href="provider.notice.apiKeyUrl"
        target="_blank"
        rel="noopener noreferrer"
        class="inline-flex justify-center rounded bg-blue-500 px-2 py-1 text-xs font-medium text-white transition-colors hover:bg-blue-600 sm:py-0.5"
      >
        Get API Key →
      </a>
    </div>

    <NoAuthProxyCard v-if="!isCustom && provider.noAuth" :provider-id="id" />
    <ConnectionsCard v-else :provider-id="id" :is-o-auth="false" />

    <ModelsCard
      v-if="showModels"
      :provider-id="id"
      :kind-filter="kind"
      :provider-alias-override="isCustom ? customNode?.prefix : undefined"
    />

    <ProviderInfoCard
      v-if="configForKind"
      :config="configForKind"
      :provider="provider"
      :title="`${kindConfig?.label} Config`"
    />

    <EmbeddingExampleCard v-if="kind === 'embedding'" :provider-id="id" :custom-alias="customNode?.prefix" />
    <GenericExampleCard v-else-if="!isCustom && KIND_EXAMPLE_CONFIG[kind]" :provider-id="id" :kind="kind" />

    <AddCustomEmbeddingModal
      v-if="isCustom"
      :is-open="showEditModal"
      :node="customNode"
      @close="showEditModal = false"
      @saved="onSaved"
    />
  </div>
</template>
