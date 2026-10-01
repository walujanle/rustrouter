<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { RouterLink, useRouter } from "vue-router";

import ModelSelectModal from "@/components/ModelSelectModal.vue";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Input from "@/components/ui/UiInput.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { useProviders } from "@/constants/providers";
import { VALID_NAME_REGEX } from "@/views/combos/utils";

// Only webSearch and webFetch combos reach this page. `saveCombo` uses PUT
// (the route accepts PUT and PATCH).

const props = defineProps<{ id: string }>();

const router = useRouter();
const { AI_PROVIDERS, MEDIA_PROVIDER_KINDS } = useProviders();

const KIND_LABELS: Record<string, string> = {
	webSearch: "Web Search",
	webFetch: "Web Fetch",
};

const EXAMPLE_PATHS: Record<string, string> = {
	webSearch: "/v1/search",
	webFetch: "/v1/web/fetch",
};

const EXAMPLE_BODIES: Record<string, (name: string) => Record<string, unknown>> = {
	webSearch: (n) => ({ model: n, query: "What is the latest news about AI?", search_type: "web", max_results: 5 }),
	webFetch: (n) => ({ model: n, url: "https://example.com", format: "markdown" }),
};

const combo = ref<Record<string, any> | null>(null);
const loading = ref(true);
const name = ref("");
const nameError = ref("");
const providers = ref<string[]>([]);
const roundRobin = ref(false);
const showPicker = ref(false);
const logs = ref<string[]>([]);
const testing = ref(false);
const testResult = ref<Record<string, any> | null>(null);
const testError = ref("");
const apiKey = ref("");
const connections = ref<Array<Record<string, any>>>([]);
const modelAliases = ref<Record<string, string>>({});

const kindLabel = computed(
	() => KIND_LABELS[combo.value?.kind] || MEDIA_PROVIDER_KINDS.find((k) => k.id === combo.value?.kind)?.label || "Combo",
);
const examplePath = computed(() => (combo.value ? EXAMPLE_PATHS[combo.value.kind] : undefined));
const exampleBody = computed(() =>
	combo.value && EXAMPLE_BODIES[combo.value.kind] ? EXAMPLE_BODIES[combo.value.kind](combo.value.name) : null,
);
const curlExample = computed(() => {
	if (!examplePath.value) return "";
	const origin =
		typeof window !== "undefined" ? window.location.origin : "http://localhost:20129";
	return `curl -X POST ${origin}${examplePath.value} \\\n  -H "Content-Type: application/json" \\\n  -H "Authorization: Bearer ${apiKey.value || "YOUR_KEY"}" \\\n  -d '${JSON.stringify(exampleBody.value)}'`;
});
const backHref = computed(() => {
	const kind = combo.value?.kind;
	if (kind === "webSearch" || kind === "webFetch") return "/dashboard/media-providers/web";
	return `/dashboard/media-providers/${kind}`;
});

function parseModelEntry(entry: string): { providerId: string; model: string } {
	if (typeof entry !== "string") return { providerId: "", model: "" };
	const idx = entry.indexOf("/");
	if (idx < 0) return { providerId: entry, model: "" };
	return { providerId: entry.slice(0, idx), model: entry.slice(idx + 1) };
}

onMounted(fetchAll);

async function fetchAll() {
	try {
		const [comboRes, settingsRes, logsRes, keysRes, connsRes, aliasesRes] = await Promise.all([
			fetch(`/api/combos/${props.id}`, { cache: "no-store" }),
			fetch("/api/settings", { cache: "no-store" }),
			fetch("/api/usage/logs", { cache: "no-store" }),
			fetch("/api/keys", { cache: "no-store" }),
			fetch("/api/providers", { cache: "no-store" }),
			fetch("/api/models/alias", { cache: "no-store" }),
		]);
		if (aliasesRes.ok) modelAliases.value = (await aliasesRes.json()).aliases || {};
		if (keysRes.ok) {
			const k = await keysRes.json();
			apiKey.value = (k.keys || []).find((x: Record<string, any>) => x.isActive !== false)?.key || "";
		}
		if (connsRes.ok) connections.value = (await connsRes.json()).connections || [];
		if (!comboRes.ok) {
			combo.value = null;
			loading.value = false;
			return;
		}
		const c = await comboRes.json();
		combo.value = c;
		name.value = c.name;
		providers.value = c.models || [];
		const s = settingsRes.ok ? await settingsRes.json() : {};
		roundRobin.value = s.comboStrategies?.[c.name]?.fallbackStrategy === "round-robin";
		const allLogs = logsRes.ok ? await logsRes.json() : [];
		logs.value = (Array.isArray(allLogs) ? allLogs : [])
			.filter((l: unknown) => typeof l === "string" && (l as string).includes(c.name))
			.slice(0, 50) as string[];
	} catch {
		/* offline */
	}
	loading.value = false;
}

function validateName(v: string): boolean {
	if (!v.trim()) {
		nameError.value = "Name is required";
		return false;
	}
	if (!VALID_NAME_REGEX.test(v)) {
		nameError.value = "Only letters, numbers, -, _ and .";
		return false;
	}
	nameError.value = "";
	return true;
}

async function saveCombo(patch: Record<string, any>): Promise<boolean> {
	const res = await fetch(`/api/combos/${props.id}`, {
		method: "PUT",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify(patch),
	});
	if (!res.ok) {
		const err = await res.json().catch(() => ({}));
		alert(err.error || "Failed to save");
		return false;
	}
	return true;
}

async function handleSaveName() {
	if (!validateName(name.value)) return;
	if (name.value === combo.value?.name) return;
	const ok = await saveCombo({ name: name.value });
	if (ok) await fetchAll();
}

async function handleAddModel(model: any) {
	const value = model?.value || model;
	if (!value || providers.value.includes(value)) return;
	const next = [...providers.value, value];
	providers.value = next;
	await saveCombo({ models: next });
}

async function handleDeselectModel(model: any) {
	const value = model?.value || model;
	if (!value || !providers.value.includes(value)) return;
	const next = providers.value.filter((p) => p !== value);
	providers.value = next;
	await saveCombo({ models: next });
}

async function handleRemoveProvider(idx: number) {
	const next = providers.value.filter((_, i) => i !== idx);
	providers.value = next;
	await saveCombo({ models: next });
}

async function handleMove(idx: number, dir: number) {
	const next = [...providers.value];
	const swap = idx + dir;
	if (swap < 0 || swap >= next.length) return;
	[next[idx], next[swap]] = [next[swap], next[idx]];
	providers.value = next;
	await saveCombo({ models: next });
}

async function handleToggleRoundRobin(enabled: boolean) {
	roundRobin.value = enabled;
	const settingsRes = await fetch("/api/settings", { cache: "no-store" });
	const s = settingsRes.ok ? await settingsRes.json() : {};
	const updated = { ...(s.comboStrategies || {}) };
	if (enabled) updated[combo.value?.name] = { fallbackStrategy: "round-robin" };
	else delete updated[combo.value?.name];
	await fetch("/api/settings", {
		method: "PATCH",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify({ comboStrategies: updated }),
	});
}

async function handleDelete() {
	if (!confirm(`Delete combo "${combo.value?.name}"?`)) return;
	const res = await fetch(`/api/combos/${props.id}`, { method: "DELETE" });
	if (res.ok) router.push(backHref.value);
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

async function handleTest() {
	testing.value = true;
	testResult.value = null;
	testError.value = "";
	const start = Date.now();
	try {
		const path = examplePath.value;
		if (!path || !combo.value) return;
		const body = EXAMPLE_BODIES[combo.value.kind](combo.value.name);
		const headers: Record<string, string> = { "Content-Type": "application/json" };
		if (apiKey.value) headers.Authorization = `Bearer ${apiKey.value}`;
		const res = await fetch(`/api${path}`, { method: "POST", headers, body: JSON.stringify(body) });
		const latencyMs = Date.now() - start;
		const data = await res.json().catch(() => ({}));
		if (!res.ok) {
			testError.value = data?.error?.message || data?.error || `HTTP ${res.status}`;
			testResult.value = { json: JSON.stringify(data, null, 2), latencyMs };
			return;
		}
		testResult.value = { json: JSON.stringify(maskB64(data), null, 2), latencyMs };
	} catch (e) {
		testError.value = e instanceof Error ? e.message : "Network error";
	} finally {
		testing.value = false;
	}
}
</script>

<template>
  <div v-if="loading" class="text-text-muted text-sm">Loading...</div>
  <div v-else-if="!combo" class="text-text-muted text-sm py-12 text-center">Combo not found.</div>

  <div v-else class="flex flex-col gap-6">
    <div class="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
      <div class="flex items-center gap-3 min-w-0">
        <RouterLink :to="backHref" class="text-text-muted hover:text-primary">
          <span class="material-symbols-outlined">arrow_back</span>
        </RouterLink>
        <div class="size-10 rounded-lg bg-primary/10 flex items-center justify-center">
          <span class="material-symbols-outlined text-primary">layers</span>
        </div>
        <div class="min-w-0">
          <p class="text-xs text-text-muted">{{ kindLabel }} Combo</p>
          <code class="text-lg font-semibold font-mono">{{ combo.name }}</code>
        </div>
      </div>
      <Button variant="outline" icon="delete" class="text-red-500 border-red-200 hover:bg-red-50" @click="handleDelete">
        Delete
      </Button>
    </div>

    <Card>
      <h2 class="text-lg font-semibold mb-3">Settings</h2>
      <div class="flex flex-col gap-4">
        <div>
          <Input
            v-model="name"
            label="Combo Name"
            :error="nameError"
            @update:model-value="validateName(name)"
            @blur="handleSaveName"
          />
          <p class="text-[10px] text-text-muted mt-0.5">Only letters, numbers, -, _ and .</p>
        </div>
        <div class="flex items-center justify-between">
          <div>
            <p class="text-sm font-medium">Round Robin</p>
            <p class="text-xs text-text-muted">Rotate providers across requests instead of strict fallback order.</p>
          </div>
          <Toggle :model-value="roundRobin" @update:model-value="handleToggleRoundRobin" />
        </div>
      </div>
    </Card>

    <Card>
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between mb-3">
        <div>
          <h2 class="text-lg font-semibold">Providers</h2>
          <p class="text-xs text-text-muted">Tried in order (top-down) or rotated when round-robin is on.</p>
        </div>
        <Button size="sm" icon="add" @click="showPicker = true">Add Provider</Button>
      </div>
      <div
        v-if="providers.length === 0"
        class="text-center py-6 border border-dashed border-border rounded-lg text-text-muted text-sm"
      >
        No providers yet.
      </div>
      <div v-else class="flex flex-col gap-2">
        <div
          v-for="(entry, idx) in providers"
          :key="`${entry}-${idx}`"
          class="flex items-center gap-3 p-2 rounded-lg bg-black/2 dark:bg-white/2"
        >
          <span class="text-xs text-text-muted w-5 text-center">{{ idx + 1 }}</span>
          <ProviderIcon
            :src="`/providers/${parseModelEntry(entry).providerId}.png`"
            :alt="AI_PROVIDERS[parseModelEntry(entry).providerId]?.name || parseModelEntry(entry).providerId"
            :size="24"
            class="object-contain rounded shrink-0"
            :fallback-text="
              AI_PROVIDERS[parseModelEntry(entry).providerId]?.textIcon ||
              parseModelEntry(entry).providerId.slice(0, 2).toUpperCase()
            "
            :fallback-color="AI_PROVIDERS[parseModelEntry(entry).providerId]?.color"
          />
          <div class="min-w-0 flex-1">
            <div class="text-sm font-medium truncate">
              {{ AI_PROVIDERS[parseModelEntry(entry).providerId]?.name || parseModelEntry(entry).providerId }}
            </div>
            <code v-if="parseModelEntry(entry).model" class="text-[10px] text-text-muted font-mono truncate block">
              {{ parseModelEntry(entry).model }}
            </code>
          </div>
          <div class="flex items-center gap-0.5">
            <button
              type="button"
              :disabled="idx === 0"
              :class="['p-1 rounded', idx === 0 ? 'text-text-muted/20' : 'text-text-muted hover:text-primary hover:bg-black/5']"
              title="Move up"
              @click="handleMove(idx, -1)"
            >
              <span class="material-symbols-outlined text-[16px]">arrow_upward</span>
            </button>
            <button
              type="button"
              :disabled="idx === providers.length - 1"
              :class="[
                'p-1 rounded',
                idx === providers.length - 1 ? 'text-text-muted/20' : 'text-text-muted hover:text-primary hover:bg-black/5',
              ]"
              title="Move down"
              @click="handleMove(idx, 1)"
            >
              <span class="material-symbols-outlined text-[16px]">arrow_downward</span>
            </button>
            <button
              type="button"
              class="p-1 rounded text-text-muted hover:text-red-500 hover:bg-red-500/10"
              title="Remove"
              @click="handleRemoveProvider(idx)"
            >
              <span class="material-symbols-outlined text-[16px]">close</span>
            </button>
          </div>
        </div>
      </div>
    </Card>

    <Card v-if="combo.kind && examplePath">
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between mb-3">
        <h2 class="text-lg font-semibold">Test Example</h2>
        <Button size="sm" icon="play_arrow" :disabled="testing || providers.length === 0" @click="handleTest">
          {{ testing ? "Running..." : "Run" }}
        </Button>
      </div>
      <pre
        class="text-xs font-mono bg-black/3 dark:bg-white/3 p-3 rounded-lg overflow-x-auto whitespace-pre-wrap break-all"
      >{{ curlExample }}</pre>
      <p v-if="testError" class="mt-3 text-xs text-red-500 wrap-break-word">{{ testError }}</p>
      <div v-if="testResult" class="mt-3 flex flex-col gap-3">
        <span v-if="testResult.latencyMs != null" class="text-[11px] text-text-muted">⚡ {{ testResult.latencyMs }}ms</span>
        <pre
          v-if="testResult.json"
          class="text-xs font-mono bg-black/3 dark:bg-white/3 p-3 rounded-lg overflow-auto max-h-75 whitespace-pre-wrap break-all"
        >{{ testResult.json }}</pre>
      </div>
    </Card>

    <Card>
      <h2 class="text-lg font-semibold mb-3">Usage Logs</h2>
      <p v-if="logs.length === 0" class="text-xs text-text-muted italic">No usage yet.</p>
      <pre
        v-else
        class="text-[11px] font-mono bg-black/3 dark:bg-white/3 p-3 rounded-lg overflow-auto max-h-100 whitespace-pre-wrap"
      >{{ logs.join("\n") }}</pre>
    </Card>

    <ModelSelectModal
      :is-open="showPicker"
      :active-providers="connections"
      :model-aliases="modelAliases"
      :title="`Add ${kindLabel} Model`"
      :kind-filter="combo.kind"
      :added-model-values="providers"
      :close-on-select="false"
      @close="showPicker = false"
      @select="handleAddModel"
      @deselect="handleDeselectModel"
    />
  </div>
</template>
