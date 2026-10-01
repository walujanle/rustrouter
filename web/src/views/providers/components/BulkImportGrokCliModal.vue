<script setup lang="ts">
import { computed, ref } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Modal from "@/components/ui/UiModal.vue";

const PLACEHOLDER = `[
  {
    "access_token": "eyJ0eXAiOiJhdCtqd3Qi...",
    "refresh_token": "LZhriF9bf88pPykpXCuZ9...",
    "id_token": "eyJ0eXAiOiJKV1QiLCJhbGci...",
    "email": "account1@example.com"
  },
  {
    "access_token": "eyJ0eXAiOiJhdCtqd3Qi...",
    "refresh_token": "LZhriF9bf88pPykpXCuZ9...",
    "id_token": "eyJ0eXAiOiJKV1QiLCJhbGci...",
    "email": "account2@example.com"
  }
]`;

function parseAccountsInput(rawText: string): Array<Record<string, any>> {
	const trimmed = rawText.trim();
	if (!trimmed) return [];

	let parsed: any;
	try {
		parsed = JSON.parse(trimmed);
	} catch (initialErr) {
		try {
			let fixed = trimmed;
			if (!fixed.startsWith("[")) {
				fixed = fixed.replace(/\}\s*,\s*\{/g, "},{").replace(/\}\s*\{/g, "},{");
				if (fixed.endsWith(",")) fixed = fixed.slice(0, -1);
				fixed = `[${fixed}]`;
			}
			parsed = JSON.parse(fixed);
		} catch {
			throw initialErr;
		}
	}

	if (Array.isArray(parsed)) {
		return parsed;
	}
	if (parsed && typeof parsed === "object") {
		if (Array.isArray(parsed.accounts)) return parsed.accounts;
		return [parsed];
	}

	throw new Error("Input must be a JSON object or array of objects");
}

const props = defineProps<{ isOpen: boolean }>();

const emit = defineEmits<{ success: []; close: [] }>();

const jsonText = ref("");
const submitting = ref(false);
const parseError = ref("");
const result = ref<Record<string, any> | null>(null);
const isDragging = ref(false);
const fileCountInfo = ref<{ filesCount: number; accountsCount: number } | null>(null);
const fileInputRef = ref<HTMLInputElement | null>(null);

const failedItems = computed(
	() => result.value?.results?.filter((r: Record<string, any>) => !r.ok) || [],
);

function handleClose() {
	if (submitting.value) return;
	jsonText.value = "";
	parseError.value = "";
	result.value = null;
	fileCountInfo.value = null;
	isDragging.value = false;
	emit("close");
}

async function processFiles(files: FileList | null) {
	if (!files || files.length === 0) return;
	parseError.value = "";
	const jsonFiles = Array.from(files).filter(
		(file) => file.name.endsWith(".json") || file.type === "application/json" || file.type === "",
	);

	if (jsonFiles.length === 0) {
		parseError.value = "Please select valid .json files";
		return;
	}

	try {
		const allAccounts: Array<Record<string, any>> = [];
		for (const file of jsonFiles) {
			const text = await file.text();
			const accountsFromFile = parseAccountsInput(text);
			if (Array.isArray(accountsFromFile)) {
				allAccounts.push(...accountsFromFile);
			} else if (accountsFromFile) {
				allAccounts.push(accountsFromFile);
			}
		}

		if (allAccounts.length === 0) {
			parseError.value = "No accounts found in selected files";
			return;
		}

		jsonText.value = JSON.stringify(allAccounts, null, 2);
		fileCountInfo.value = {
			filesCount: jsonFiles.length,
			accountsCount: allAccounts.length,
		};
	} catch (err) {
		parseError.value = `Error reading files: ${(err as Error).message}`;
	}
}

function handleFileInputChange(e: Event) {
	const input = e.target as HTMLInputElement;
	processFiles(input.files);
	input.value = "";
}

function handleDragOver(e: DragEvent) {
	e.preventDefault();
	isDragging.value = true;
}

function handleDragLeave(e: DragEvent) {
	e.preventDefault();
	isDragging.value = false;
}

function handleDrop(e: DragEvent) {
	e.preventDefault();
	isDragging.value = false;
	if (e.dataTransfer?.files?.length && e.dataTransfer.files.length > 0) {
		processFiles(e.dataTransfer.files);
	}
}

async function handleSubmit() {
	parseError.value = "";
	result.value = null;

	let accounts: Array<Record<string, any>>;
	try {
		accounts = parseAccountsInput(jsonText.value);
	} catch (err) {
		parseError.value = `Invalid JSON: ${(err as Error).message}`;
		return;
	}

	if (!accounts || accounts.length === 0) {
		parseError.value = "No accounts found in input";
		return;
	}

	submitting.value = true;
	try {
		const res = await fetch("/api/oauth/grok-cli/bulk-import", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ accounts }),
		});
		const data = await res.json();

		if (!res.ok) {
			parseError.value = data?.error || `Request failed: ${res.status}`;
			return;
		}

		result.value = data;
		if (data.success > 0) emit("success");
	} catch (err) {
		parseError.value = (err as Error).message || "Request failed";
	} finally {
		submitting.value = false;
	}
}
</script>

<template>
  <Modal :is-open="props.isOpen" title="Bulk Add Grok CLI Accounts" @close="handleClose">
    <div class="flex flex-col gap-4">
      <div class="flex flex-wrap items-center justify-between gap-2">
        <p class="text-xs text-text-muted">
          Upload multiple .json files or paste JSON array / object.
        </p>
        <input
          ref="fileInputRef"
          type="file"
          accept=".json,application/json"
          multiple
          class="hidden"
          @change="handleFileInputChange"
        />
        <Button
          type="button"
          size="sm"
          variant="secondary"
          icon="upload_file"
          :disabled="submitting"
          @click="fileInputRef?.click()"
        >
          Upload JSON Files
        </Button>
      </div>

      <div
        :class="`relative rounded border transition-colors ${isDragging ? 'border-primary bg-primary/10 ring-2 ring-primary/30' : 'border-accent/30 bg-sidebar'}`"
      >
        <textarea
          v-model="jsonText"
          class="w-full rounded bg-transparent p-2.5 text-sm font-mono resize-y min-h-60 focus:outline-none focus:ring-1 focus:ring-primary"
          :placeholder="PLACEHOLDER"
          :disabled="submitting"
          @input="fileCountInfo = null"
          @dragover="handleDragOver"
          @dragleave="handleDragLeave"
          @drop="handleDrop"
        />

        <div
          v-if="isDragging"
          class="absolute inset-0 flex flex-col items-center justify-center bg-sidebar/90 rounded pointer-events-none backdrop-blur-xs"
        >
          <span class="material-symbols-outlined text-3xl text-primary mb-1">upload_file</span>
          <span class="text-sm font-medium text-primary">
            Drop .json files here
          </span>
        </div>
      </div>

      <div
        v-if="fileCountInfo"
        class="flex items-center gap-1.5 text-xs text-green-400 font-medium bg-green-500/10 border border-green-500/20 px-2.5 py-1.5 rounded"
      >
        <span class="material-symbols-outlined text-sm">check_circle</span>
        <span>
          Loaded {{ fileCountInfo.accountsCount }} account(s) from
          {{ fileCountInfo.filesCount }} file(s)
        </span>
      </div>

      <p v-if="parseError" class="text-xs text-red-500 wrap-break-word">{{ parseError }}</p>

      <div v-if="result && result.failed > 0" class="flex flex-col gap-2">
        <div class="text-sm font-medium text-yellow-400">
          ✗ {{ result.failed }} failed
        </div>
        <ul v-if="failedItems.length > 0" class="rounded border border-accent/20 bg-sidebar/50 p-2 text-xs font-mono max-h-40 overflow-y-auto">
          <li v-for="item in failedItems" :key="item.index" class="text-red-400">
            [{{ item.index }}] {{ item.error }}
          </li>
        </ul>
      </div>

      <div class="flex gap-2">
        <Button
          :disabled="submitting || !jsonText.trim()"
          full-width
          @click="handleSubmit"
        >
          {{ submitting ? "Importing..." : "Import All" }}
        </Button>
        <Button variant="ghost" full-width :disabled="submitting" @click="handleClose">
          Close
        </Button>
      </div>
    </div>
  </Modal>
</template>
