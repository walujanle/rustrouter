<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useRouter } from "vue-router";

import AddCustomEmbeddingModal from "@/components/AddCustomEmbeddingModal.vue";
import Button from "@/components/ui/UiButton.vue";
import { useProviders } from "@/constants/providers";
import MediaProviderCard from "./components/MediaProviderCard.vue";

// The webSearch and webFetch listings live on the merged `/web` page, so both
// redirect there.
//
// No kind offers Create Combo here: image/tts combos are hidden, and the /web
// page owns the search/fetch combos.

const props = defineProps<{ kind: string }>();

const router = useRouter();
const { MEDIA_PROVIDER_KINDS, getProvidersByKind } = useProviders();

const connections = ref<Array<Record<string, any>>>([]);
const customNodes = ref<Array<Record<string, any>>>([]);
const showAddCustomEmbedding = ref(false);

const kindConfig = computed(() => MEDIA_PROVIDER_KINDS.find((k) => k.id === props.kind));
const isEmbedding = computed(() => props.kind === "embedding");
const isWebKind = computed(() => props.kind === "webSearch" || props.kind === "webFetch");

const providers = computed(() => getProvidersByKind(props.kind));
const customProviders = computed(() =>
	customNodes.value.map((n) => ({
		id: n.id,
		name: n.name || "Custom Embedding",
		color: "#6366F1",
		textIcon: "CE",
	})),
);
const allProviders = computed(() => [...providers.value, ...customProviders.value]);

async function fetchData() {
	const tasks: Array<Promise<void>> = [
		fetch("/api/providers", { cache: "no-store" })
			.then((r) => r.json())
			.then((d) => {
				connections.value = d.connections || [];
			})
			.catch(() => {}),
	];
	if (isEmbedding.value) {
		tasks.push(
			fetch("/api/provider-nodes", { cache: "no-store" })
				.then((r) => r.json())
				.then((d) => {
					customNodes.value = (d.nodes || []).filter(
						(n: Record<string, any>) => n.type === "custom-embedding",
					);
				})
				.catch(() => {}),
		);
	}
	await Promise.all(tasks);
}

onMounted(() => {
	if (isWebKind.value) {
		router.replace("/dashboard/media-providers/web");
		return;
	}
	fetchData();
});

watch(
	() => props.kind,
	(kind) => {
		if (kind === "webSearch" || kind === "webFetch") {
			router.replace("/dashboard/media-providers/web");
			return;
		}
		fetchData();
	},
);

async function handleToggleProvider(providerId: string, newActive: boolean) {
	const providerConns = connections.value.filter((c) => c.provider === providerId);
	connections.value = connections.value.map((c) => (c.provider === providerId ? { ...c, isActive: newActive } : c));
	await Promise.allSettled(
		providerConns.map((c) =>
			fetch(`/api/providers/${c.id}`, {
				method: "PUT",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ isActive: newActive }),
			}),
		),
	);
}

function onCustomCreated(node: Record<string, any>) {
	customNodes.value = [...customNodes.value, node];
	showAddCustomEmbedding.value = false;
}
</script>

<template>
  <div v-if="!kindConfig" class="text-text-muted text-sm py-12 text-center">
    Unknown media kind "{{ kind }}".
  </div>

  <div v-else-if="!isWebKind" class="flex flex-col gap-6">
    <div v-if="isEmbedding" class="flex items-center justify-end gap-2">
      <Button size="sm" icon="add" @click="showAddCustomEmbedding = true">Add Custom Embedding</Button>
    </div>

    <div
      v-if="allProviders.length === 0"
      class="text-center py-12 border border-dashed border-border rounded-xl text-text-muted text-sm"
    >
      No providers support <strong>{{ kindConfig.label }}</strong> yet.
    </div>
    <div v-else class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
      <MediaProviderCard
        v-for="provider in providers"
        :key="provider.id"
        :provider="provider"
        :kind="kind"
        :connections="connections"
        @toggle="handleToggleProvider"
      />
      <MediaProviderCard
        v-for="provider in customProviders"
        :key="provider.id"
        :provider="provider"
        :kind="kind"
        :connections="connections"
        is-custom
        @toggle="handleToggleProvider"
      />
    </div>

    <AddCustomEmbeddingModal
      :is-open="showAddCustomEmbedding"
      @close="showAddCustomEmbedding = false"
      @created="onCustomCreated"
    />
  </div>
</template>
