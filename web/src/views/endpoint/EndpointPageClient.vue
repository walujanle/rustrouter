<script setup lang="ts">
import { onMounted, ref } from "vue";
import CardSkeleton from "@/components/ui/CardSkeleton.vue";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";
import { useSettingsStore } from "@/stores/settings";
import EndpointRow from "./components/EndpointRow.vue";
import SecurityWarning from "./components/SecurityWarning.vue";

interface ApiKey {
	id: string;
	name: string;
	key: string;
	createdAt: string;
	isActive?: boolean;
	machineId?: string;
}

interface ConfirmState {
	title: string;
	message: string;
	onConfirm: () => void;
}

const settingsStore = useSettingsStore();
const { copied, copy } = useCopyToClipboard();

const keys = ref<ApiKey[]>([]);
const loading = ref(true);
const showAddModal = ref(false);
const newKeyName = ref("");
const createdKey = ref<string | null>(null);
const confirmState = ref<ConfirmState | null>(null);

const requireApiKey = ref(false);
const requireLogin = ref(true);
const hasPassword = ref(true);

// API key visibility toggle state
const visibleKeys = ref<Set<string>>(new Set());

// Client-side local/remote detection (UI hint only, not a security gate)
const isRemoteHost = ref(false);

const baseUrl = ref("/v1");

async function fetchData() {
	try {
		const fetchKeys = async (): Promise<ApiKey[]> => {
			const res = await fetch("/api/keys");
			if (!res.ok) return [];
			const data = await res.json();
			return data.keys || [];
		};

		let existing = await fetchKeys();
		// Auto-provision a default key for first-time users so the endpoint works out of the box.
		if (existing.length === 0) {
			try {
				const createRes = await fetch("/api/keys", {
					method: "POST",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify({ name: "Default Key" }),
				});
				if (createRes.ok) existing = await fetchKeys();
			} catch {
				/* fall through to empty render */
			}
		}
		keys.value = existing;
	} catch (error) {
		console.log("Error fetching data:", error);
	} finally {
		loading.value = false;
	}
}

async function loadSettings() {
	try {
		const settingsData = await settingsStore.fetchSettings();
		if (settingsData) {
			requireApiKey.value = settingsData.requireApiKey || false;
			requireLogin.value = settingsData.requireLogin !== false;
			hasPassword.value = settingsData.hasPassword || false;
		}
	} catch (error) {
		console.log("Error loading settings:", error);
	}
}

async function handleRequireApiKey(value: boolean) {
	try {
		const updated = await settingsStore.patchSettings({ requireApiKey: value });
		if (updated) requireApiKey.value = value;
	} catch (error) {
		console.log("Error updating requireApiKey:", error);
	}
}

async function handleCreateKey() {
	if (!newKeyName.value.trim()) return;

	try {
		const res = await fetch("/api/keys", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ name: newKeyName.value }),
		});
		const data = await res.json();

		if (res.ok) {
			createdKey.value = data.key;
			await fetchData();
			newKeyName.value = "";
			showAddModal.value = false;
		}
	} catch (error) {
		console.log("Error creating key:", error);
	}
}

function handleDeleteKey(id: string) {
	confirmState.value = {
		title: "Delete API Key",
		message: "Delete this API key?",
		onConfirm: async () => {
			confirmState.value = null;
			try {
				const res = await fetch(`/api/keys/${id}`, { method: "DELETE" });
				if (res.ok) {
					keys.value = keys.value.filter((k) => k.id !== id);
					const next = new Set(visibleKeys.value);
					next.delete(id);
					visibleKeys.value = next;
				}
			} catch (error) {
				console.log("Error deleting key:", error);
			}
		},
	};
}

async function handleToggleKey(id: string, isActive: boolean) {
	try {
		const res = await fetch(`/api/keys/${id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ isActive }),
		});
		if (res.ok) {
			keys.value = keys.value.map((k) => (k.id === id ? { ...k, isActive } : k));
		}
	} catch (error) {
		console.log("Error toggling key:", error);
	}
}

function handleKeyToggleValue(key: ApiKey, checked: boolean) {
	handleKeyToggle(key, checked);
}

function handleKeyToggle(key: ApiKey, checked: boolean) {
	if (key.isActive && !checked) {
		confirmState.value = {
			title: "Pause API Key",
			message: `Pause API key "${key.name}"?\n\nThis key will stop working immediately but can be resumed later.`,
			onConfirm: async () => {
				confirmState.value = null;
				handleToggleKey(key.id, checked);
			},
		};
	} else {
		handleToggleKey(key.id, checked);
	}
}

function maskKey(fullKey: string): string {
	if (!fullKey || fullKey.length <= 10) return fullKey || "";
	return fullKey.slice(0, 6) + "•".repeat(fullKey.length - 10) + fullKey.slice(-4);
}

function toggleKeyVisibility(keyId: string) {
	const next = new Set(visibleKeys.value);
	if (next.has(keyId)) next.delete(keyId);
	else next.add(keyId);
	visibleKeys.value = next;
}

function closeAddModal() {
	showAddModal.value = false;
	newKeyName.value = "";
}

function copyCreatedKey() {
	if (createdKey.value) copy(createdKey.value, "created_key");
}

function handleConfirm() {
	confirmState.value?.onConfirm();
}

onMounted(() => {
	if (typeof window !== "undefined") {
		isRemoteHost.value = !["localhost", "127.0.0.1", "::1"].includes(window.location.hostname);
		baseUrl.value = `${window.location.origin}/v1`;
	}
	fetchData();
	loadSettings();
});
</script>

<template>
  <div class="flex flex-col gap-8">
    <template v-if="loading">
      <CardSkeleton />
      <CardSkeleton />
    </template>

    <template v-else>
      <!-- Endpoint Card -->
      <Card>
        <h2 class="text-lg font-semibold mb-4 flex items-center gap-2">
          <span class="material-symbols-outlined text-primary">api</span>
          API Endpoint
        </h2>

        <!-- Endpoint rows -->
        <div class="flex flex-col gap-2">
          <!-- Local -->
          <EndpointRow
            label="Local"
            :url="baseUrl"
            copy-id="local_url"
            :copied="copied"
            @copy="copy"
          />
        </div>
      </Card>

      <!-- API Keys -->
      <Card id="require-api-key">
        <div class="flex items-center justify-between mb-4">
          <h2 class="text-lg font-semibold flex items-center gap-2">
            <span class="material-symbols-outlined text-primary">vpn_key</span>
            API Keys
          </h2>
          <Button icon="add" @click="showAddModal = true">Create Key</Button>
        </div>

        <div class="flex items-center justify-between pb-4 mb-4 border-b border-border">
          <div>
            <p class="font-medium">Require API key</p>
            <p class="text-sm text-text-muted">Requests without a valid key will be rejected</p>
          </div>
          <Toggle :model-value="requireApiKey" @update:model-value="handleRequireApiKey" />
        </div>

        <div v-if="isRemoteHost && !requireApiKey" class="mb-4 -mt-2">
          <SecurityWarning message="Endpoint is exposed without an API key." />
        </div>

        <div v-if="keys.length === 0" class="text-center py-12">
          <div class="inline-flex items-center justify-center w-16 h-16 rounded-full bg-primary/10 text-primary mb-4">
            <span class="material-symbols-outlined text-[32px]">vpn_key</span>
          </div>
          <p class="text-text-main font-medium mb-1">No API keys yet</p>
          <p class="text-sm text-text-muted mb-4">Create your first API key to get started</p>
          <Button icon="add" @click="showAddModal = true">Create Key</Button>
        </div>

        <div v-else class="flex flex-col">
          <div
            v-for="apiKey in keys"
            :key="apiKey.id"
            class="group flex items-center justify-between py-3 border-b border-black/3 dark:border-white/3 last:border-b-0"
            :class="apiKey.isActive === false ? 'opacity-60' : ''"
          >
            <div class="flex-1 min-w-0">
              <p class="text-sm font-medium">{{ apiKey.name }}</p>
              <div class="flex items-center gap-2 mt-1">
                <code class="text-xs text-text-muted font-mono">{{ visibleKeys.has(apiKey.id) ? apiKey.key : maskKey(apiKey.key) }}</code>
                <button
                  type="button"
                  class="p-1 hover:bg-black/5 dark:hover:bg-white/5 rounded text-text-muted hover:text-primary transition-all"
                  :title="visibleKeys.has(apiKey.id) ? 'Hide key' : 'Show key'"
                  @click="toggleKeyVisibility(apiKey.id)"
                >
                  <span class="material-symbols-outlined text-[14px]">
                    {{ visibleKeys.has(apiKey.id) ? "visibility_off" : "visibility" }}
                  </span>
                </button>
                <button
                  type="button"
                  :aria-label="copied === apiKey.id ? 'Copied' : 'Copy API key'"
                  :title="copied === apiKey.id ? 'Copied' : 'Copy API key'"
                  class="p-1 hover:bg-black/5 dark:hover:bg-white/5 rounded text-text-muted hover:text-primary transition-all"
                  @click="copy(apiKey.key, apiKey.id)"
                >
                  <span class="material-symbols-outlined text-[14px]">
                    {{ copied === apiKey.id ? "check" : "content_copy" }}
                  </span>
                </button>
              </div>
              <p class="text-xs text-text-muted mt-1">
                Created {{ new Date(apiKey.createdAt).toLocaleDateString("en-US") }}
              </p>
              <p v-if="apiKey.isActive === false" class="text-xs text-orange-500 mt-1">Paused</p>
            </div>
            <div class="flex items-center gap-2">
              <Toggle
                size="sm"
                :model-value="apiKey.isActive ?? true"
                @update:model-value="handleKeyToggleValue(apiKey, $event)"
              />
              <button
                type="button"
                aria-label="Delete API key"
                title="Delete API key"
                class="p-2 hover:bg-red-500/10 rounded text-red-500 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 transition-all"
                @click="handleDeleteKey(apiKey.id)"
              >
                <span class="material-symbols-outlined text-[18px]">delete</span>
              </button>
            </div>
          </div>
        </div>
      </Card>

      <!-- Add Key Modal -->
      <Modal :is-open="showAddModal" title="Create API Key" @close="closeAddModal">
        <div class="flex flex-col gap-4">
          <Input v-model="newKeyName" label="Key Name" placeholder="Production Key" />
          <div class="flex gap-2">
            <Button :disabled="!newKeyName.trim()" full-width @click="handleCreateKey">Create</Button>
            <Button variant="ghost" full-width @click="closeAddModal">Cancel</Button>
          </div>
        </div>
      </Modal>

      <!-- Created Key Modal -->
      <Modal :is-open="!!createdKey" title="API Key Created" @close="createdKey = null">
        <div class="flex flex-col gap-4">
          <div class="bg-yellow-50 dark:bg-yellow-900/20 border border-yellow-200 dark:border-yellow-800 rounded-lg p-4">
            <p class="text-sm text-yellow-800 dark:text-yellow-200 mb-2 font-medium">Save this key now!</p>
            <p class="text-sm text-yellow-700 dark:text-yellow-300">
              This is the only time you will see this key. Store it securely.
            </p>
          </div>
          <div class="flex gap-2">
            <Input :model-value="createdKey || ''" readonly class-name="flex-1 font-mono text-sm" />
            <Button
              variant="secondary"
              :icon="copied === 'created_key' ? 'check' : 'content_copy'"
              @click="copyCreatedKey"
            >
              {{ copied === "created_key" ? "Copied!" : "Copy" }}
            </Button>
          </div>
          <Button full-width @click="createdKey = null">Done</Button>
        </div>
      </Modal>

      <!-- Confirm Modal -->
      <ConfirmModal
        :is-open="!!confirmState"
        :title="confirmState?.title || 'Confirm'"
        :message="confirmState?.message"
        variant="danger"
        @close="confirmState = null"
        @confirm="handleConfirm"
      />
    </template>
  </div>
</template>
