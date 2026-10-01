<script setup lang="ts">
import { computed, onMounted, ref, watch, watchEffect } from "vue";

import ComboFormModal from "@/components/ComboFormModal.vue";
import CardSkeleton from "@/components/ui/CardSkeleton.vue";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Select from "@/components/ui/UiSelect.vue";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";
import { useModelCaps } from "@/hooks/useModelCaps";
import CapacityAdapterSection from "@/views/combos/components/CapacityAdapterSection.vue";
import ComboCard from "@/views/combos/components/ComboCard.vue";
import type { CapEntry, Combo } from "@/views/combos/utils";
import {
	CAPACITY_ADAPTER_CAPS,
	EMPTY_CAPACITY_ADAPTER,
	normalizeCapEntry,
	STRATEGY_OPTIONS,
} from "@/views/combos/utils";

interface ConfirmState {
	title?: string;
	message?: string;
	confirmText?: string;
	variant?: "primary" | "danger" | "success";
	loading?: boolean;
	onConfirm?: () => Promise<void> | void;
}

const combos = ref<Combo[]>([]);
const loading = ref(true);
const showCreateModal = ref(false);
const editingCombo = ref<Combo | null>(null);
const activeProviders = ref<Array<Record<string, any>>>([]);
const comboStrategies = ref<Record<string, { fallbackStrategy?: string; judgeModel?: string }>>({});
const capacityAdapter = ref<Record<string, CapEntry>>({ ...EMPTY_CAPACITY_ADAPTER });
const confirmState = ref<ConfirmState | null>(null);
const presetLoading = ref<string | null>(null); // "cursor" | "claude" | null
const selectedIds = ref<string[]>([]);
const bulkBusy = ref(false);
const { copied, copy } = useCopyToClipboard();
const { getCaps } = useModelCaps();
const selectAllBox = ref<HTMLInputElement | null>(null);

onMounted(() => {
	fetchData();
});

// Drop stale selection when the combo list changes (delete / refresh).
watch(combos, () => {
	const alive = new Set(combos.value.map((c) => c.id));
	selectedIds.value = selectedIds.value.filter((id) => alive.has(id));
});

const selectedCombos = computed(() => combos.value.filter((c) => selectedIds.value.includes(c.id)));
const allSelected = computed(() => combos.value.length > 0 && selectedIds.value.length === combos.value.length);
const someSelected = computed(() => selectedIds.value.length > 0);

watchEffect(() => {
	if (selectAllBox.value) selectAllBox.value.indeterminate = someSelected.value && !allSelected.value;
});

function toggleSelect(id: string) {
	selectedIds.value = selectedIds.value.includes(id)
		? selectedIds.value.filter((x) => x !== id)
		: [...selectedIds.value, id];
}

function toggleSelectAll() {
	selectedIds.value = allSelected.value ? [] : combos.value.map((c) => c.id);
}

function clearSelection() {
	selectedIds.value = [];
}

async function handleGeneratePresets(source: string) {
	const label = source === "cursor" ? "Cursor Default" : "Claude Default";
	presetLoading.value = source;
	try {
		const previewRes = await fetch(`/api/combos/presets?source=${source}`);
		const preview = await previewRes.json();
		if (!previewRes.ok) {
			alert(preview.error || `Failed to preview ${label}`);
			return;
		}

		const toCreate = preview.toCreate ?? (preview.items || []).filter((i: any) => !i.exists).length;
		const toSkip = preview.toSkip ?? (preview.items || []).filter((i: any) => i.exists).length;
		const total = (preview.items || []).length;

		if (total === 0) {
			alert(`No ${label} models available to generate.`);
			return;
		}

		if (toCreate === 0) {
			alert(`All ${total} ${label} combos already exist. Nothing to create.`);
			return;
		}

		confirmState.value = {
			title: `Generate ${label}`,
			message: `Create ${toCreate} combo${toCreate === 1 ? "" : "s"} named like ${source === "cursor" ? "Cursor" : "Claude"} model IDs (seeded with cu/… or cc/…). ${toSkip} already exist and will be skipped. You can edit any combo afterward to add fallbacks.`,
			confirmText: "Generate",
			variant: "primary",
			onConfirm: async () => {
				if (confirmState.value) confirmState.value = { ...confirmState.value, loading: true };
				try {
					const res = await fetch("/api/combos/presets", {
						method: "POST",
						headers: { "Content-Type": "application/json" },
						body: JSON.stringify({ source }),
					});
					const data = await res.json();
					if (!res.ok) {
						alert(data.error || `Failed to generate ${label}`);
						return;
					}
					await fetchData();
					confirmState.value = null;
				} catch (error) {
					console.log(`Error generating ${label}:`, error);
					alert(`Failed to generate ${label}`);
					if (confirmState.value) confirmState.value = { ...confirmState.value, loading: false };
				}
			},
		};
	} catch (error) {
		console.log(`Error previewing ${label}:`, error);
		alert(`Failed to preview ${label}`);
	} finally {
		presetLoading.value = null;
	}
}

async function fetchData() {
	try {
		const [combosRes, providersRes, settingsRes] = await Promise.all([
			fetch("/api/combos"),
			fetch("/api/providers"),
			fetch("/api/settings"),
		]);
		const combosData = await combosRes.json();
		const providersData = await providersRes.json();
		const settingsData = settingsRes.ok ? await settingsRes.json() : {};

		// Only LLM combos here - webSearch/webFetch combos belong to media-providers/web
		if (combosRes.ok) {
			combos.value = (combosData.combos || []).filter((c: Combo) => !c.kind || c.kind === "llm");
		}
		if (providersRes.ok) {
			activeProviders.value = providersData.connections || [];
		}
		comboStrategies.value = settingsData.comboStrategies || {};
		const rawAdapter = settingsData.capacityAdapter || {};
		const normalized: Record<string, CapEntry> = {};
		for (const cap of CAPACITY_ADAPTER_CAPS) {
			normalized[cap.key] = normalizeCapEntry(rawAdapter[cap.key]);
		}
		capacityAdapter.value = normalized;
	} catch (error) {
		console.log("Error fetching data:", error);
	} finally {
		loading.value = false;
	}
}

async function handleSetCapacityAdapter(next: Record<string, CapEntry>) {
	capacityAdapter.value = next;
	try {
		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ capacityAdapter: next }),
		});
	} catch (error) {
		console.log("Error updating capacity adapter:", error);
	}
}

async function handleCreate(data: { name: string; models: string[] }) {
	try {
		const res = await fetch("/api/combos", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(data),
		});
		if (res.ok) {
			await fetchData();
			showCreateModal.value = false;
		} else {
			const err = await res.json();
			alert(err.error || "Failed to create combo");
		}
	} catch (error) {
		console.log("Error creating combo:", error);
	}
}

async function handleUpdate(id: string, data: { name: string; models: string[] }) {
	try {
		const res = await fetch(`/api/combos/${id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(data),
		});
		if (res.ok) {
			await fetchData();
			editingCombo.value = null;
		} else {
			const err = await res.json();
			alert(err.error || "Failed to update combo");
		}
	} catch (error) {
		console.log("Error updating combo:", error);
	}
}

function pruneStrategiesForNames(
	names: string[],
	base: Record<string, { fallbackStrategy?: string; judgeModel?: string }> = comboStrategies.value,
) {
	const updated = { ...base };
	for (const name of names) delete updated[name];
	return updated;
}

async function persistComboStrategies(updated: Record<string, { fallbackStrategy?: string; judgeModel?: string }>) {
	await fetch("/api/settings", {
		method: "PATCH",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify({ comboStrategies: updated }),
	});
	comboStrategies.value = updated;
}

function handleDelete(id: string) {
	const combo = combos.value.find((c) => c.id === id);
	confirmState.value = {
		title: "Delete Combo",
		message: combo ? `Delete combo "${combo.name}"?` : "Delete this combo?",
		onConfirm: async () => {
			if (confirmState.value) confirmState.value = { ...confirmState.value, loading: true };
			try {
				const res = await fetch(`/api/combos/${id}`, { method: "DELETE" });
				if (res.ok) {
					if (combo?.name) {
						await persistComboStrategies(pruneStrategiesForNames([combo.name]));
					}
					combos.value = combos.value.filter((c) => c.id !== id);
					selectedIds.value = selectedIds.value.filter((x) => x !== id);
				}
				confirmState.value = null;
			} catch (error) {
				console.log("Error deleting combo:", error);
				if (confirmState.value) confirmState.value = { ...confirmState.value, loading: false };
			}
		},
	};
}

function handleBulkDelete() {
	if (selectedCombos.value.length === 0) return;
	const count = selectedCombos.value.length;
	confirmState.value = {
		title: "Delete Selected Combos",
		message: `Delete ${count} selected combo${count === 1 ? "" : "s"}? This cannot be undone.`,
		confirmText: "Delete",
		variant: "danger",
		onConfirm: async () => {
			if (confirmState.value) confirmState.value = { ...confirmState.value, loading: true };
			bulkBusy.value = true;
			try {
				const ids = selectedCombos.value.map((c) => c.id);
				const names = selectedCombos.value.map((c) => c.name);
				const results = await Promise.all(
					ids.map((id) => fetch(`/api/combos/${id}`, { method: "DELETE" })),
				);
				const failed = results.filter((r) => !r.ok).length;
				await persistComboStrategies(pruneStrategiesForNames(names));
				combos.value = combos.value.filter((c) => !ids.includes(c.id));
				clearSelection();
				confirmState.value = null;
				if (failed > 0) alert(`Deleted with ${failed} failure${failed === 1 ? "" : "s"}.`);
			} catch (error) {
				console.log("Error bulk deleting combos:", error);
				alert("Failed to delete selected combos");
				if (confirmState.value) confirmState.value = { ...confirmState.value, loading: false };
			} finally {
				bulkBusy.value = false;
			}
		},
	};
}

// Merge a per-combo strategy patch into settings.comboStrategies. Passing an empty
// patch (strategy back to default "fallback") drops the entry entirely.
async function handleSetComboStrategy(comboName: string, patch: { fallbackStrategy?: string; judgeModel?: string }) {
	try {
		const updated = { ...comboStrategies.value };
		const next = { ...(updated[comboName] || {}), ...patch };
		// Prune to keep settings clean: default fallback with no extras = no entry.
		if (!next.fallbackStrategy || next.fallbackStrategy === "fallback") {
			delete updated[comboName];
		} else {
			updated[comboName] = next;
		}

		await persistComboStrategies(updated);
	} catch (error) {
		console.log("Error updating combo strategy:", error);
	}
}

async function handleBulkSetStrategy(strategy: string) {
	if (selectedCombos.value.length === 0 || !strategy) return;
	bulkBusy.value = true;
	try {
		const updated = { ...comboStrategies.value };
		for (const combo of selectedCombos.value) {
			if (!strategy || strategy === "fallback") {
				delete updated[combo.name];
			} else {
				updated[combo.name] = {
					...(updated[combo.name] || {}),
					fallbackStrategy: strategy,
				};
			}
		}
		await persistComboStrategies(updated);
	} catch (error) {
		console.log("Error bulk updating combo strategy:", error);
		alert("Failed to update strategy for selected combos");
	} finally {
		bulkBusy.value = false;
	}
}

const comboByName = computed(() => Object.fromEntries(combos.value.map((c) => [c.name, c.models])));

function onBulkStrategy(value: string) {
	if (value) handleBulkSetStrategy(value);
}

function onEditSave(data: { name: string; models: string[] }) {
	const combo = editingCombo.value;
	if (combo) handleUpdate(combo.id, data);
}

function closeConfirm() {
	if (!confirmState.value?.loading) confirmState.value = null;
}

function onConfirmAction() {
	confirmState.value?.onConfirm?.();
}
</script>

<template>
  <div v-if="loading" class="flex flex-col gap-6">
    <CardSkeleton />
    <CardSkeleton />
  </div>

  <div v-else class="flex min-w-0 flex-col gap-6 px-1 sm:px-0">
    <!-- Header -->
    <div class="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
      <div class="min-w-0">
        <p class="text-sm text-text-muted mt-1">
          Group models under one name, then pick a strategy per combo:
        </p>
        <ul class="text-sm text-text-muted mt-2 flex flex-col gap-1">
          <li><span class="font-medium text-text-main">Fallback</span> — tries models in order (next on failure)</li>
          <li><span class="font-medium text-text-main">Round Robin</span> — rotates models across requests to spread load</li>
          <li><span class="font-medium text-text-main">Fusion</span> — queries all models in parallel, then a judge synthesizes one answer. Best quality, but costs the most: every request bills all panel models + the judge (N+1 calls)</li>
        </ul>
        <p class="hidden text-xs text-text-muted mt-3 max-w-2xl">
          <span class="font-medium text-text-main">Cursor / Claude Default</span> create combos named exactly like those clients&apos; model IDs (e.g. <code class="font-mono">composer-2.5</code>, <code class="font-mono">opus</code>), seeded with the matching <code class="font-mono">cu/…</code> or <code class="font-mono">cc/…</code> route so traffic can hit RustRouter without the prefix.
          {{ " " }}Note: Cursor IDE itself often blocks built-in Composer / Grok from Override OpenAI Base URL (&quot;model does not support custom API&quot;); add them via Cursor&apos;s <span class="font-medium text-text-main">Add Custom Model</span> using the combo name, or pick a model Cursor allows through the custom endpoint.
        </p>
      </div>
      <div class="flex w-full flex-col gap-2 sm:w-auto sm:items-stretch">
        <Button icon="add" class="w-full sm:w-auto whitespace-nowrap" @click="showCreateModal = true">
          Create Combo
        </Button>
        <div class="hidden">
          <Button
            variant="secondary"
            size="sm"
            icon="edit_note"
            :loading="presetLoading === 'cursor'"
            :disabled="!!presetLoading"
            class="w-full whitespace-nowrap"
            @click="handleGeneratePresets('cursor')"
          >
            Cursor Default
          </Button>
          <Button
            variant="secondary"
            size="sm"
            icon="smart_toy"
            :loading="presetLoading === 'claude'"
            :disabled="!!presetLoading"
            class="w-full whitespace-nowrap"
            @click="handleGeneratePresets('claude')"
          >
            Claude Default
          </Button>
        </div>
      </div>
    </div>

    <!-- Combos List -->
    <Card v-if="combos.length === 0">
      <div class="text-center py-12">
        <div class="inline-flex items-center justify-center w-16 h-16 rounded-full bg-primary/10 text-primary mb-4">
          <span class="material-symbols-outlined text-[32px]">layers</span>
        </div>
        <p class="text-text-main font-medium mb-1">No combos yet</p>
        <p class="text-sm text-text-muted mb-4">Create model combos with fallback support</p>
        <Button icon="add" class="w-full sm:w-auto" @click="showCreateModal = true">
          Create Combo
        </Button>
      </div>
    </Card>
    <div v-else class="flex flex-col gap-3">
      <!-- Selection toolbar -->
      <div class="flex min-w-0 flex-col gap-2 rounded-lg border border-black/5 bg-black/1.5 px-3 py-2 dark:border-white/5 dark:bg-white/2 sm:flex-row sm:items-center sm:justify-between">
        <label class="flex cursor-pointer items-center gap-2 text-xs text-text-muted hover:text-primary select-none">
          <input
            ref="selectAllBox"
            type="checkbox"
            :checked="allSelected"
            class="h-3.5 w-3.5 rounded border-gray-300 text-primary focus:ring-primary"
            @change="toggleSelectAll"
          />
          <span>
            {{ someSelected ? `${selectedIds.length} selected` : `Select all (${combos.length})` }}
          </span>
        </label>

        <div class="flex min-w-0 flex-wrap items-center gap-2">
          <template v-if="someSelected">
            <div class="w-full min-w-40 sm:w-50">
              <Select
                :options="STRATEGY_OPTIONS"
                :model-value="''"
                placeholder="Set strategy…"
                :disabled="bulkBusy"
                select-class-name="py-1.5 text-xs"
                @update:model-value="onBulkStrategy"
              />
            </div>
            <Button
              size="sm"
              variant="danger"
              icon="delete"
              :disabled="bulkBusy"
              :loading="bulkBusy"
              class="whitespace-nowrap"
              @click="handleBulkDelete"
            >
              Delete ({{ selectedIds.length }})
            </Button>
            <Button size="sm" variant="ghost" :disabled="bulkBusy" @click="clearSelection">
              Clear
            </Button>
          </template>
        </div>
      </div>

      <div class="flex flex-col gap-3">
        <ComboCard
          v-for="combo in combos"
          :key="combo.id"
          :combo="combo"
          :get-caps="getCaps"
          :combo-by-name="comboByName"
          :active-providers="activeProviders"
          :copied="copied"
          :strategy="comboStrategies[combo.name] || {}"
          :selected="selectedIds.includes(combo.id)"
          @copy="copy"
          @edit="editingCombo = combo"
          @delete="handleDelete(combo.id)"
          @toggle-select="toggleSelect(combo.id)"
          @set-strategy="(patch) => handleSetComboStrategy(combo.name, patch)"
        />
      </div>
    </div>

    <!-- Capacity Adapter -->
    <CapacityAdapterSection
      :capacity-adapter="capacityAdapter"
      :active-providers="activeProviders"
      :get-caps="getCaps"
      @change="handleSetCapacityAdapter"
    />

    <!-- Create Modal - Use key to force remount and reset state -->
    <ComboFormModal
      v-if="showCreateModal"
      key="create"
      :is-open="showCreateModal"
      :active-providers="activeProviders"
      @close="showCreateModal = false"
      @save="handleCreate"
    />

    <ComboFormModal
      v-if="editingCombo"
      :key="editingCombo.id"
      :is-open="!!editingCombo"
      :combo="editingCombo"
      :active-providers="activeProviders"
      @close="editingCombo = null"
      @save="onEditSave"
    />

    <!-- Confirm (delete / generate presets) -->
    <ConfirmModal
      :is-open="!!confirmState"
      :title="confirmState?.title || 'Confirm'"
      :message="confirmState?.message"
      :confirm-text="confirmState?.confirmText || 'Confirm'"
      :variant="confirmState?.variant || 'danger'"
      :loading="!!confirmState?.loading"
      @close="closeConfirm"
      @confirm="onConfirmAction"
    />
  </div>
</template>
