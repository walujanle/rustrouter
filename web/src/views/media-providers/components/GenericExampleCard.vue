<script setup lang="ts">
import { computed, onMounted, ref } from "vue";

import Card from "@/components/ui/UiCard.vue";
import { getModelKind, useModels } from "@/constants/models";
import { useProviders } from "@/constants/providers";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";
import { KIND_EXAMPLE_CONFIG } from "./exampleShared";
import Row from "./MediaRow.vue";

// Pruned to the kept kinds (webSearch, webFetch, systemone). Dropped with their
// modalities: the whole image path (edit/mask defaults, binary output, codex SSE
// progress previews), the tunnel toggle, and imageToText (no routed endpoint).

const props = defineProps<{ providerId: string; kind: string }>();

const { getProviderAlias, resolveProviderId, MEDIA_PROVIDER_KINDS } = useProviders();
const { getModelsByProviderId } = useModels();
const { copied: copiedCurl, copy: copyCurl } = useCopyToClipboard();
const { copied: copiedRes, copy: copyRes } = useCopyToClipboard();

const kindConfig = computed(() => MEDIA_PROVIDER_KINDS.find((k) => k.id === props.kind));
const exConfig = computed(() => KIND_EXAMPLE_CONFIG[props.kind] || null);

// A provider alias that round-trips through resolveProviderId is the safe one to
// put in the request; otherwise fall back to the raw id.
const safeProviderAlias = computed(() => {
	const alias = getProviderAlias(props.providerId);
	return resolveProviderId(alias) === props.providerId ? alias : props.providerId;
});

const kindModels = computed(() =>
	getModelsByProviderId(props.providerId).filter((m) => getModelKind(m) === props.kind),
);
// Kinds that carry a model identifier in the request.
const KIND_NEEDS_MODEL = new Set(["systemone"]);
const needsModel = computed(() => KIND_NEEDS_MODEL.has(props.kind));
const allowManualModel = computed(() => needsModel.value && kindModels.value.length === 0);

const selectedModel = ref("");
const input = ref("");
const question = ref("Does this request require urgent attention?");
const extraValues = ref<Record<string, any>>({});
const apiKey = ref("");
const localEndpoint = ref("");
const result = ref<Record<string, any> | null>(null);
const running = ref(false);
const error = ref("");
const connections = ref<Array<Record<string, any>>>([]);
const pinnedConnectionId = ref("");

const endpoint = computed(() => localEndpoint.value);
const apiPath = computed(() => kindConfig.value?.endpoint?.path || "");
const modelFull = computed(() => {
	if (!needsModel.value) return safeProviderAlias.value;
	if (selectedModel.value) return `${safeProviderAlias.value}/${selectedModel.value}`;
	return allowManualModel.value ? "" : safeProviderAlias.value;
});

const requestBody = computed<Record<string, any>>(() => {
	const cfg = exConfig.value;
	if (!cfg) return {};
	const extras: Record<string, any> = {};
	for (const [k, v] of Object.entries(extraValues.value)) {
		if (v === "" || v === null || v === undefined) continue;
		if (typeof v === "number" && Number.isNaN(v)) continue;
		extras[k] = v;
	}
	const systemoneQuestions =
		props.kind === "systemone"
			? {
					questions: {
						is_urgent: {
							type: "noul",
							instructions: question.value.trim() || "Does this request require urgent attention?",
						},
					},
				}
			: {};
	return {
		model: modelFull.value,
		[cfg.bodyKey]: input.value,
		...cfg.extraBody,
		...extras,
		...systemoneQuestions,
	};
});

const curlSnippet = computed(() => {
	const headers = `-H "Content-Type: application/json" \\\n  -H "Authorization: Bearer ${apiKey.value || "YOUR_KEY"}"${
		pinnedConnectionId.value ? ` \\\n  -H "x-connection-id: ${pinnedConnectionId.value}"` : ""
	}`;
	return `curl -X POST ${endpoint.value}${apiPath.value} \\\n  ${headers} \\\n  -d '${JSON.stringify(requestBody.value)}'`;
});

onMounted(() => {
	localEndpoint.value = window.location.origin;
	selectedModel.value = kindModels.value[0]?.id ?? "";
	input.value = exConfig.value?.defaultInput || "";
	extraValues.value = (exConfig.value?.extraFields || []).reduce<Record<string, any>>((acc, f) => {
		acc[f.key] = f.default ?? "";
		return acc;
	}, {});
	fetch("/api/keys")
		.then((r) => r.json())
		.then((d) => {
			apiKey.value = (d.keys || []).find((k: Record<string, any>) => k.isActive !== false)?.key || "";
		})
		.catch(() => {});
	fetch("/api/providers/client")
		.then((r) => r.json())
		.then((d) => {
			connections.value = (d.connections || []).filter(
				(c: Record<string, any>) => c.provider === props.providerId && c.isActive !== false,
			);
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
		if (pinnedConnectionId.value) headers["x-connection-id"] = pinnedConnectionId.value;
		const res = await fetch(`/api${apiPath.value}`, {
			method: "POST",
			headers,
			body: JSON.stringify(requestBody.value),
		});
		if (!res.ok) {
			const data = await res.json().catch(() => ({}));
			error.value = data?.error?.message || data?.error || `HTTP ${res.status}`;
			return;
		}
		const data = await res.json();
		result.value = { data, latencyMs: Date.now() - start };
	} catch (e) {
		error.value = e instanceof Error ? e.message : "Network error";
	} finally {
		running.value = false;
	}
}

function maskB64(obj: any): any {
	if (!obj || typeof obj !== "object") return obj;
	if (Array.isArray(obj)) return obj.map(maskB64);
	const out: Record<string, any> = {};
	for (const [k, v] of Object.entries(obj)) {
		out[k] = k === "b64_json" && typeof v === "string" && v.length > 100 ? `<${v.length} chars base64>` : maskB64(v);
	}
	return out;
}

const resultJson = computed(() => (result.value ? JSON.stringify(maskB64(result.value.data), null, 2) : ""));

// Extra fields are filtered by the selected model's declared params, if any.
const visibleExtraFields = computed(() => {
	const fields = exConfig.value?.extraFields || [];
	const selected = kindModels.value.find((m) => m.id === selectedModel.value);
	if (kindModels.value.length === 0) return fields;
	return fields.filter((f) => Array.isArray(selected?.params) && selected.params.includes(f.key));
});
</script>

<template>
  <Card v-if="kindConfig && exConfig">
    <h2 class="text-lg font-semibold mb-4">Example</h2>
    <div class="flex flex-col gap-2.5">
      <Row v-if="kindModels.length > 0" label="Model">
        <select
          v-model="selectedModel"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
        >
          <option v-for="m in kindModels" :key="m.id" :value="m.id">{{ m.name || m.id }}</option>
        </select>
      </Row>
      <Row v-else-if="allowManualModel" label="Model">
        <input
          v-model="selectedModel"
          placeholder="Enter model id (provider-specific)"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary font-mono"
        />
      </Row>

      <Row label="Endpoint">
        <span class="w-full min-w-0 flex-1 px-3 py-1.5 text-sm font-mono text-text-main bg-sidebar rounded-lg truncate">
          {{ endpoint }}{{ apiPath }}
        </span>
      </Row>

      <Row label="API Key">
        <span class="px-3 py-1.5 text-sm font-mono text-text-main bg-sidebar rounded-lg truncate block">
          {{
            apiKey
              ? `${apiKey.slice(0, 8)}${"•".repeat(Math.min(20, Math.max(0, apiKey.length - 8)))}`
              : "No key configured"
          }}
        </span>
      </Row>

      <Row v-if="connections.length > 0" label="Connection">
        <select
          v-model="pinnedConnectionId"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
        >
          <option value="">Auto (by priority)</option>
          <option v-for="c in connections" :key="c.id" :value="c.id">
            {{ c.email || c.name || String(c.id).slice(0, 8) }}
          </option>
        </select>
      </Row>

      <Row :label="exConfig.inputLabel">
        <div class="relative">
          <input
            v-model="input"
            :placeholder="exConfig.inputPlaceholder"
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

      <Row v-if="kind === 'systemone'" label="Question">
        <input
          v-model="question"
          placeholder="Enter evaluation question or criteria"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
        />
      </Row>

      <Row v-for="f in visibleExtraFields" :key="f.key" :label="f.label">
        <select
          v-if="f.type === 'select'"
          :value="extraValues[f.key] ?? ''"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
          @change="extraValues[f.key] = ($event.target as HTMLSelectElement).value"
        >
          <option v-for="opt in f.options || []" :key="opt" :value="opt">{{ opt === "" ? "(default)" : opt }}</option>
        </select>
        <input
          v-else-if="f.type === 'text'"
          type="text"
          :value="extraValues[f.key] ?? ''"
          :placeholder="f.placeholder"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
          @input="extraValues[f.key] = ($event.target as HTMLInputElement).value"
        />
        <input
          v-else
          type="number"
          :value="extraValues[f.key] ?? ''"
          :min="f.min"
          :max="f.max"
          class="w-full px-3 py-1.5 text-sm border border-border rounded-lg bg-surface focus:outline-none focus:border-primary"
          @input="
            extraValues[f.key] =
              ($event.target as HTMLInputElement).value === '' ? '' : Number(($event.target as HTMLInputElement).value)
          "
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

      <p v-if="error" class="text-xs text-red-500 break-words">{{ error }}</p>

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
        <pre class="bg-sidebar rounded-lg px-3 py-2.5 text-xs font-mono text-text-main overflow-x-auto whitespace-pre-wrap break-all opacity-70">{{ result ? resultJson : exConfig.defaultResponse }}</pre>
      </div>
    </div>
  </Card>
</template>
