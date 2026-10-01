<script setup lang="ts">
import { computed, ref } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Modal from "@/components/ui/UiModal.vue";

const PLACEHOLDER = `[
  {
    "accessToken": "eyJhbGc...",
    "refreshToken": "rt_...",
    "idToken": "eyJhbGc...",
    "email": "user@example.com"
  }
]`;

function normalizeToArray(parsed: any): Array<Record<string, any>> | null {
	if (Array.isArray(parsed)) return parsed;
	if (parsed && typeof parsed === "object") {
		if (Array.isArray(parsed.accounts)) return parsed.accounts;
		return [parsed];
	}
	return null;
}

const props = defineProps<{ isOpen: boolean }>();

const emit = defineEmits<{ success: []; close: [] }>();

const jsonText = ref("");
const submitting = ref(false);
const parseError = ref("");
const result = ref<Record<string, any> | null>(null);

function handleClose() {
	if (submitting.value) return;
	jsonText.value = "";
	parseError.value = "";
	result.value = null;
	emit("close");
}

async function handleSubmit() {
	parseError.value = "";
	result.value = null;

	const trimmed = jsonText.value.trim();
	if (!trimmed) return;

	let parsed: any;
	try {
		parsed = JSON.parse(trimmed);
	} catch (err) {
		parseError.value = `Invalid JSON: ${(err as Error).message}`;
		return;
	}

	const accounts = normalizeToArray(parsed);
	if (!accounts || accounts.length === 0) {
		parseError.value = "No accounts found in input";
		return;
	}

	submitting.value = true;
	try {
		const res = await fetch("/api/oauth/codex/bulk-import", {
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

const failedItems = computed(
	() => result.value?.results?.filter((r: Record<string, any>) => !r.ok) || [],
);
</script>

<template>
  <Modal :is-open="props.isOpen" title="Bulk Add Codex Accounts" @close="handleClose">
    <div class="flex flex-col gap-4">
      <p class="text-xs text-text-muted">
        Paste an array of codex account JSON objects. Each must include accessToken (and ideally refreshToken, idToken).
      </p>

      <textarea
        v-model="jsonText"
        class="w-full rounded border border-accent/30 bg-sidebar p-2 text-sm font-mono resize-y min-h-60 focus:outline-none focus:ring-1 focus:ring-primary"
        :placeholder="PLACEHOLDER"
        :disabled="submitting"
      />

      <p v-if="parseError" class="text-xs text-red-500 break-words">{{ parseError }}</p>

      <div v-if="result" class="flex flex-col gap-2">
        <div
          :class="`text-sm font-medium ${result.failed > 0 ? 'text-yellow-400' : 'text-green-400'}`"
        >
          ✓ {{ result.success }} added{{ result.failed > 0 ? `, ✗ ${result.failed} failed` : "" }}
        </div>
        <ul v-if="failedItems.length > 0" class="rounded border border-accent/20 bg-sidebar/50 p-2 text-xs font-mono max-h-40 overflow-y-auto">
          <li v-for="item in failedItems" :key="item.index" class="text-red-400">
            [{{ item.index }}] {{ item.error }}
          </li>
        </ul>
      </div>

      <div class="flex gap-2">
        <Button
          full-width
          :disabled="submitting || !jsonText.trim()"
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
