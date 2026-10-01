<script setup lang="ts">
import { VueMonacoEditor } from "@guolao/vue-monaco-editor";
import { defineComponent, h, ref } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";

// 7 steps matching requestLogger files exactly
const STEPS = [
	{ id: 1, label: "Client Request", file: "1_req_client.json", lang: "json", desc: "Raw request from client" },
	{ id: 2, label: "Source Body", file: "2_req_source.json", lang: "json", desc: "After initial conversion" },
	{ id: 3, label: "OpenAI Intermediate", file: "3_req_openai.json", lang: "json", desc: "source → openai" },
	{ id: 4, label: "Target Request", file: "4_req_target.json", lang: "json", desc: "openai → target + URL + headers" },
	{ id: 5, label: "Provider Response", file: "5_res_provider.txt", lang: "text", desc: "Raw SSE from provider" },
	{ id: 6, label: "OpenAI Response", file: "6_res_openai.txt", lang: "text", desc: "target → openai (response)" },
	{ id: 7, label: "Client Response", file: "7_res_client.txt", lang: "text", desc: "Final response to client" },
];

const EDITOR_OPTIONS = {
	minimap: { enabled: false },
	fontSize: 12,
	lineNumbers: "on",
	scrollBeyondLastLine: false,
	wordWrap: "on",
	automaticLayout: true,
} as const;

const META_COLORS: Record<string, string> = {
	blue: "bg-blue-500/10 text-blue-500",
	orange: "bg-orange-500/10 text-orange-500",
	green: "bg-green-500/10 text-green-500",
	purple: "bg-purple-500/10 text-purple-500",
};

const MetaBadge = defineComponent({
	props: {
		label: { type: String, required: true },
		value: { type: String, default: "" },
		color: { type: String, required: true },
	},
	setup(props) {
		return () =>
			h(
				"span",
				{
					class: `inline-flex items-center gap-1 px-2 py-0.5 rounded text-xs font-mono ${META_COLORS[props.color]}`,
				},
				[
					h("span", { class: "text-text-muted/70 font-sans text-[10px]" }, `${props.label}:`),
					props.value,
				],
			);
	},
});

interface TranslatorMeta {
	provider?: string;
	model?: string;
	sourceFormat?: string;
	targetFormat?: string;
}

const contents = ref<Record<number, string>>({});
const expanded = ref<Record<number, boolean>>({ 1: true });
const loading = ref<Record<string, boolean>>({});
// Detected from step 1: { provider, model, sourceFormat, targetFormat }
const meta = ref<TranslatorMeta | null>(null);

function setLoad(key: string, val: boolean) {
	loading.value = { ...loading.value, [key]: val };
}
function setContent(id: number, val: string) {
	contents.value = { ...contents.value, [id]: val };
}
function toggle(id: number) {
	expanded.value = { ...expanded.value, [id]: !expanded.value[id] };
}
function openNext(nextId: number) {
	const next: Record<number, boolean> = {};
	for (const s of STEPS) next[s.id] = false;
	next[nextId] = true;
	expanded.value = next;
}

function isExpanded(id: number): boolean {
	return !!expanded.value[id];
}
function contentOf(id: number): string {
	return contents.value[id] || "";
}

// Load file from logs/translator/
async function handleLoad(stepId: number) {
	const step = STEPS.find((s) => s.id === stepId);
	if (!step) return;
	setLoad(`load-${stepId}`, true);
	try {
		const res = await fetch(`/api/translator/load?file=${step.file}`);
		const data = await res.json();
		if (data.success) {
			setContent(stepId, data.content);
			if (stepId === 1) await detectMeta(data.content);
		} else {
			alert(data.error || "File not found");
		}
	} catch (e) {
		alert((e as Error).message);
	}
	setLoad(`load-${stepId}`, false);
}

// Step 1: detect provider/format from model field
async function detectMeta(rawContent: string) {
	try {
		const body = typeof rawContent === "string" ? JSON.parse(rawContent) : rawContent;
		const res = await fetch("/api/translator/translate", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ step: 1, body }),
		});
		const data = await res.json();
		if (data.success) meta.value = data.result;
	} catch {
		/* ignore */
	}
}

function save(file: string, content: string) {
	return fetch("/api/translator/save", {
		method: "POST",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify({ file, content }),
	}).catch(() => {});
}

// Step 1 → Step 3: source → OpenAI intermediate
async function handleToOpenAI() {
	setLoad("toOpenAI", true);
	try {
		const raw = contents.value[1];
		const body = JSON.parse(raw);
		// Save input: 1_req_client.json + 2_req_source.json (body only)
		save("1_req_client.json", raw);
		save(
			"2_req_source.json",
			JSON.stringify({ timestamp: new Date().toISOString(), headers: {}, body: body.body || body }, null, 2),
		);

		const res = await fetch("/api/translator/translate", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ step: 2, body }),
		});
		const data = await res.json();
		if (!data.success) {
			alert(data.error);
			return;
		}
		const str = JSON.stringify(data.result.body, null, 2);
		setContent(3, str);
		openNext(3);
	} catch (e) {
		alert((e as Error).message);
	}
	setLoad("toOpenAI", false);
}

// Step 3 → Step 4: OpenAI → target + build URL/headers
async function handleToTarget() {
	setLoad("toTarget", true);
	try {
		const raw = contents.value[3];
		const openaiBody = JSON.parse(raw);
		// Save input: 3_req_openai.json
		save("3_req_openai.json", raw);

		const res = await fetch("/api/translator/translate", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				step: 3,
				body: { ...openaiBody, provider: meta.value?.provider, model: meta.value?.model },
			}),
		});
		const data = await res.json();
		if (!data.success) {
			alert(data.error);
			return;
		}
		// Embed provider + model so Send works even without meta
		const step4Content = { ...data.result, provider: meta.value?.provider, model: meta.value?.model };
		setContent(4, JSON.stringify(step4Content, null, 2));
		openNext(4);
	} catch (e) {
		alert((e as Error).message);
	}
	setLoad("toTarget", false);
}

// Step 4 → Step 5: send to provider via executor
async function handleSend() {
	setLoad("send", true);
	try {
		const raw = contents.value[4];
		const step4 = JSON.parse(raw);
		// Save input: 4_req_target.json
		save("4_req_target.json", raw);

		// Read provider/model from step4 content (embedded during build), fallback to meta
		const provider = step4.provider || meta.value?.provider;
		const model = step4.model || meta.value?.model;

		if (!provider || !model) {
			alert("Missing provider or model. Please run step 1 first to detect them.");
			return;
		}

		const res = await fetch("/api/translator/send", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ provider, model, body: step4.body || step4 }),
		});

		if (!res.ok) {
			const err = await res.json().catch(() => ({ error: res.statusText }));
			alert(err.error || "Send failed");
			return;
		}

		// Accumulate streaming response
		const reader = res.body?.getReader();
		if (!reader) return;
		const decoder = new TextDecoder();
		let full = "";
		while (true) {
			const { done, value } = await reader.read();
			if (done) break;
			full += decoder.decode(value, { stream: true });
		}

		setContent(5, full);
		openNext(5);

		// Save to logs/translator/5_res_provider.txt
		await fetch("/api/translator/save", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ file: "5_res_provider.txt", content: full }),
		});
	} catch (e) {
		alert((e as Error).message);
	} finally {
		setLoad("send", false);
	}
}

const { copy } = useCopyToClipboard();

function handleCopy(id: number) {
	const content = contents.value[id];
	if (!content) return;
	copy(content, `translator-step-${id}`);
}

function handleFormat(id: number) {
	try {
		const obj = JSON.parse(contents.value[id]);
		setContent(id, JSON.stringify(obj, null, 2));
	} catch {
		/* not JSON, skip */
	}
}

function handleEditorChange(id: number, value: string | undefined) {
	setContent(id, value || "");
	if (id === 1) detectMeta(value || "");
}
</script>

<template>
  <div class="p-8 space-y-3">
    <!-- Header -->
    <div class="flex items-center justify-between mb-2">
      <div>
        <h1 class="text-2xl font-bold text-text-main">Translator Debug</h1>
        <p class="text-sm text-text-muted mt-1">Replay request flow — matches log files</p>
      </div>
      <div v-if="meta" class="flex items-center gap-2 flex-wrap justify-end">
        <MetaBadge label="src" :value="meta.sourceFormat" color="blue" />
        <span class="material-symbols-outlined text-text-muted text-[14px]">arrow_forward</span>
        <MetaBadge label="dst" :value="meta.targetFormat" color="orange" />
        <MetaBadge label="provider" :value="meta.provider" color="green" />
        <MetaBadge label="model" :value="meta.model" color="purple" />
      </div>
    </div>

    <Card v-for="step in STEPS" :key="step.id">
      <div class="p-4 space-y-3">
        <!-- Step header -->
        <div class="flex items-center justify-between">
          <button
            type="button"
            class="flex items-center gap-2 flex-1 text-left group"
            @click="toggle(step.id)"
          >
            <span class="material-symbols-outlined text-[20px] text-text-muted group-hover:text-primary transition-colors">
              {{ isExpanded(step.id) ? "expand_more" : "chevron_right" }}
            </span>
            <span class="text-xs font-mono text-text-muted/60 w-4">{{ step.id }}</span>
            <h3 class="text-sm font-semibold text-text-main">{{ step.label }}</h3>
            <span class="text-xs text-text-muted/60 font-mono">{{ step.file }}</span>
            <span v-if="contentOf(step.id)" class="text-xs text-green-500">({{ contentOf(step.id).length }} chars)</span>
          </button>
          <div v-if="!isExpanded(step.id)" class="flex gap-1 shrink-0">
            <Button
              size="sm"
              variant="ghost"
              icon="folder_open"
              :loading="loading[`load-${step.id}`]"
              @click="handleLoad(step.id)"
            />
            <Button
              v-if="step.id === 1"
              size="sm"
              icon="arrow_forward"
              :loading="loading.toOpenAI"
              @click="handleToOpenAI"
            >
              → OpenAI
            </Button>
            <Button
              v-else-if="step.id === 3"
              size="sm"
              icon="arrow_forward"
              :loading="loading.toTarget"
              @click="handleToTarget"
            >
              → Target
            </Button>
            <Button v-else-if="step.id === 4" size="sm" icon="send" :loading="loading.send" @click="handleSend">
              Send
            </Button>
          </div>
        </div>

        <!-- Expanded content -->
        <template v-if="isExpanded(step.id)">
          <div class="border border-border rounded-lg overflow-hidden">
            <VueMonacoEditor
              height="400px"
              :language="step.lang === 'text' ? 'plaintext' : 'json'"
              :value="contentOf(step.id)"
              theme="vs-dark"
              :options="EDITOR_OPTIONS"
              @update:value="handleEditorChange(step.id, $event)"
            />
          </div>
          <div class="flex gap-2 flex-wrap">
            <Button
              size="sm"
              variant="outline"
              icon="folder_open"
              :loading="loading[`load-${step.id}`]"
              @click="handleLoad(step.id)"
            >
              Load
            </Button>
            <Button size="sm" variant="outline" icon="data_object" @click="handleFormat(step.id)">Format</Button>
            <Button size="sm" variant="outline" icon="content_copy" @click="handleCopy(step.id)">Copy</Button>
            <Button
              v-if="step.id === 1"
              size="sm"
              icon="arrow_forward"
              :loading="loading.toOpenAI"
              @click="handleToOpenAI"
            >
              → OpenAI
            </Button>
            <Button
              v-else-if="step.id === 3"
              size="sm"
              icon="arrow_forward"
              :loading="loading.toTarget"
              @click="handleToTarget"
            >
              → Target
            </Button>
            <Button v-else-if="step.id === 4" size="sm" icon="send" :loading="loading.send" @click="handleSend">
              Send
            </Button>
          </div>
        </template>
      </div>
    </Card>
  </div>
</template>
