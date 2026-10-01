<script setup lang="ts">
import { computed, onMounted, ref } from "vue";

import Card from "@/components/ui/UiCard.vue";
import { getModelKind, useModels } from "@/constants/models";
import { isCustomEmbeddingProvider, useProviders } from "@/constants/providers";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";
import Row from "./MediaRow.vue";

const DEFAULT_RESPONSE_EXAMPLE = `{
  "object": "list",
  "data": [{
    "object": "embedding",
    "index": 0,
    "embedding": [0.002301, -0.019212, 0.004815, -0.031249, ...]
  }],
  "model": "...",
  "usage": { "prompt_tokens": 9, "total_tokens": 9 }
}`;

const props = defineProps<{ providerId: string; customAlias?: string }>();

const { getProviderAlias } = useProviders();
const { getModelsByProviderId } = useModels();
const { copied: copiedCurl, copy: copyCurl } = useCopyToClipboard();
const { copied: copiedRes, copy: copyRes } = useCopyToClipboard();

const isCustom = computed(() => isCustomEmbeddingProvider(props.providerId));
const providerAlias = computed(() =>
	isCustom.value ? props.customAlias || props.providerId : getProviderAlias(props.providerId),
);
const embeddingModels = computed(() =>
	isCustom.value
		? []
		: getModelsByProviderId(props.providerId).filter((m) => getModelKind(m) === "embedding"),
);

const selectedModel = ref("");
const input = ref("The quick brown fox jumps over the lazy dog");
const dimensions = ref("");
const apiKey = ref("");
const localEndpoint = ref("");
const result = ref<Record<string, any> | null>(null);
const running = ref(false);
const error = ref("");

const endpoint = computed(() => localEndpoint.value);
const modelFull = computed(() => (selectedModel.value ? `${providerAlias.value}/${selectedModel.value}` : ""));

function buildBody(): Record<string, any> {
	const body: Record<string, any> = { model: modelFull.value, input: input.value.trim() };
	const dim = Number(dimensions.value);
	if (dimensions.value && Number.isFinite(dim) && dim > 0) body.dimensions = dim;
	return body;
}

const curlSnippet = computed(
	() =>
		`curl -X POST ${endpoint.value}/v1/embeddings \\\n  -H "Content-Type: application/json" \\\n  -H "Authorization: Bearer ${apiKey.value || "YOUR_KEY"}" \\\n  -d '${JSON.stringify(buildBody())}'`,
);

onMounted(() => {
	localEndpoint.value = window.location.origin;
	selectedModel.value = embeddingModels.value[0]?.id ?? "";
	fetch("/api/keys")
		.then((r) => r.json())
		.then((d) => {
			apiKey.value = (d.keys || []).find((k: Record<string, any>) => k.isActive !== false)?.key || "";
		})
		.catch(() => {});
});

async function handleRun() {
	if (!input.value.trim() || !modelFull.value) return;
	running.value = true;
	error.value = "";
	result.value = null;
	const start = Date.now();
	try {
		const headers: Record<string, string> = { "Content-Type": "application/json" };
		if (apiKey.value) headers.Authorization = `Bearer ${apiKey.value}`;
		const res = await fetch("/api/v1/embeddings", {
			method: "POST",
			headers,
			body: JSON.stringify(buildBody()),
		});
		const latencyMs = Date.now() - start;
		const data = await res.json();
		if (!res.ok) {
			error.value = data?.error?.message || data?.error || `HTTP ${res.status}`;
			return;
		}
		result.value = { data, latencyMs };
	} catch (e) {
		error.value = e instanceof Error ? e.message : "Network error";
	} finally {
		running.value = false;
	}
}

// Compact the embedding array to its first 4 values plus a dim count.
function formatResultJson(data: Record<string, any> | null | undefined): string {
	if (!data) return DEFAULT_RESPONSE_EXAMPLE;
	const clone = JSON.parse(JSON.stringify(data));
	for (const item of clone.data || []) {
		if (Array.isArray(item.embedding) && item.embedding.length > 4) {
			item.embedding = [
				...item.embedding.slice(0, 4).map((v: number) => Number.parseFloat(v.toFixed(6))),
				`... (${item.embedding.length} dims)`,
			];
		}
	}
	return JSON.stringify(clone, null, 2);
}

const resultJson = computed(() => (result.value ? JSON.stringify(result.value.data, null, 2) : ""));
</script>

<template>
  <Card>
    <h2 class="text-lg font-semibold mb-4">Example</h2>
    <div class="flex flex-col gap-2.5">
      <Row label="Model">
        <input
          v-if="isCustom"
          v-model="selectedModel"
          placeholder="e.g. voyage-3, embed-english-v3.0, text-embedding-3-small"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary font-mono"
        />
        <select
          v-else
          v-model="selectedModel"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
        >
          <option v-for="m in embeddingModels" :key="m.id" :value="m.id">{{ m.name || m.id }}</option>
        </select>
      </Row>

      <Row label="Endpoint">
        <input
          v-model="localEndpoint"
          class="w-full min-w-0 flex-1 px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary font-mono"
          placeholder="http://localhost:20129"
        />
      </Row>

      <Row label="API Key">
        <input
          v-model="apiKey"
          type="password"
          placeholder="sk-..."
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary font-mono"
        />
      </Row>

      <Row label="Input">
        <div class="relative">
          <input
            v-model="input"
            class="w-full px-3 py-1.5 pr-7 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
          />
          <button
            v-if="input"
            type="button"
            aria-label="Clear input"
            title="Clear input"
            class="absolute right-2 top-1/2 -translate-y-1/2 text-text-muted hover:text-primary transition-colors"
            @click="input = ''"
          >
            <span class="material-symbols-outlined text-[14px]">close</span>
          </button>
        </div>
      </Row>

      <Row label="Dimensions">
        <input
          v-model="dimensions"
          type="number"
          min="1"
          placeholder="optional, e.g. 512, 1024 (leave empty for default)"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
        />
      </Row>

      <div class="mt-1">
        <div class="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between mb-1.5">
          <span class="text-xs font-semibold text-text-muted uppercase tracking-wider">Request</span>
          <div class="flex w-full flex-col gap-2 sm:w-auto sm:flex-row sm:items-center">
            <button
              type="button"
              class="inline-flex items-center gap-1 text-xs text-text-muted hover:text-primary transition-colors"
              @click="copyCurl(curlSnippet)"
            >
              <span class="material-symbols-outlined text-[14px]">{{ copiedCurl ? "check" : "content_copy" }}</span>
              {{ copiedCurl ? "Copied" : "Copy" }}
            </button>
            <button
              type="button"
              :disabled="running || !input.trim() || !modelFull"
              class="flex w-full sm:w-auto items-center justify-center gap-1.5 px-3 py-1 rounded-lg bg-primary text-white text-xs font-medium hover:bg-primary/90 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
              @click="handleRun"
            >
              <span class="material-symbols-outlined text-[14px]">play_arrow</span>
              {{ running ? "Running..." : "Run" }}
            </button>
          </div>
        </div>
        <pre class="bg-sidebar rounded-lg px-3 py-2.5 text-xs font-mono text-text-main overflow-x-auto whitespace-pre-wrap break-all">{{ curlSnippet }}</pre>
      </div>

      <p v-if="error" class="text-xs text-red-500 wrap-break-word">{{ error }}</p>

      <div>
        <div class="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between mb-1.5">
          <span class="text-xs font-semibold text-text-muted uppercase tracking-wider">
            Response <span v-if="result" class="font-normal normal-case">&#9889; {{ result.latencyMs }}ms</span>
          </span>
          <button
            v-if="result"
            type="button"
            class="inline-flex items-center gap-1 text-xs text-text-muted hover:text-primary transition-colors"
            @click="copyRes(resultJson)"
          >
            <span class="material-symbols-outlined text-[14px]">{{ copiedRes ? "check" : "content_copy" }}</span>
            {{ copiedRes ? "Copied" : "Copy" }}
          </button>
        </div>
        <pre class="bg-sidebar rounded-lg px-3 py-2.5 text-xs font-mono text-text-main overflow-x-auto whitespace-pre-wrap break-all opacity-70">{{ formatResultJson(result?.data) }}</pre>
      </div>
    </div>
  </Card>
</template>
