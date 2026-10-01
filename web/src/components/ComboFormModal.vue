<script setup lang="ts">
import { ref, useId, watch } from "vue";
import { VueDraggable } from "vue-draggable-plus";

import ModelSelectModal from "@/components/ModelSelectModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";

const VALID_NAME_REGEX = /^[a-zA-Z0-9_.-]+$/;

interface Combo {
	name?: string;
	models?: string[];
}

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		combo?: Combo | null;
		activeProviders?: Array<Record<string, any>>;
		kindFilter?: string | null;
		forcePrefix?: string;
		title?: string;
	}>(),
	{ forcePrefix: "", kindFilter: null, activeProviders: () => [] },
);

const emit = defineEmits<{ close: []; save: [data: { name: string; models: string[] }] }>();

// Strip prefix when editing an existing combo so the user only edits the suffix.
const initialName = props.combo?.name
	? props.forcePrefix && props.combo.name.startsWith(props.forcePrefix)
		? props.combo.name.slice(props.forcePrefix.length)
		: props.combo.name
	: "";

const uid = useId();
const name = ref(initialName);
const models = ref<string[]>(props.combo?.models || []);
const showModelSelect = ref(false);
const saving = ref(false);
const nameError = ref("");
const modelAliases = ref<Record<string, string>>({});

// Inline model-item editing state (one item at a time).
const editingIndex = ref<number | null>(null);
const draft = ref("");

watch(
	() => props.isOpen,
	(open) => {
		if (!open) return;
		fetch("/api/models/alias")
			.then((r) => (r.ok ? r.json() : null))
			.then((d) => {
				if (d) modelAliases.value = d.aliases || {};
			})
			.catch(() => {});
	},
	{ immediate: true },
);

function validateName(value: string) {
	if (!value.trim()) {
		nameError.value = "Name is required";
		return false;
	}
	const full = props.forcePrefix + value;
	if (!VALID_NAME_REGEX.test(full)) {
		nameError.value = "Only letters, numbers, -, _ and . allowed";
		return false;
	}
	nameError.value = "";
	return true;
}

function handleNameChange(value: string) {
	let next = value;
	// If the user types the prefix manually, strip it (we always prepend)
	if (props.forcePrefix && next.startsWith(props.forcePrefix)) next = next.slice(props.forcePrefix.length);
	name.value = next;
	if (next) validateName(next);
	else nameError.value = "";
}

function startEdit(index: number) {
	editingIndex.value = index;
	draft.value = models.value[index];
}

function commitEdit() {
	const index = editingIndex.value;
	if (index === null) return;
	const trimmed = draft.value.trim();
	if (trimmed && trimmed !== models.value[index]) {
		const a = [...models.value];
		a[index] = trimmed;
		models.value = a;
	} else {
		draft.value = models.value[index];
	}
	editingIndex.value = null;
}

function handleItemKeyDown(e: KeyboardEvent) {
	if (e.key === "Enter") commitEdit();
	if (e.key === "Escape") {
		const index = editingIndex.value;
		if (index !== null) draft.value = models.value[index];
		editingIndex.value = null;
	}
}

function handleAddModel(model: any) {
	if (!models.value.includes(model.value)) models.value = [...models.value, model.value];
}

function handleDeselectModel(model: any) {
	models.value = models.value.filter((m) => m !== model.value);
}

function handleRemoveModel(i: number) {
	models.value = models.value.filter((_, idx) => idx !== i);
}

function handleMoveUp(i: number) {
	if (i === 0) return;
	const a = [...models.value];
	[a[i - 1], a[i]] = [a[i], a[i - 1]];
	models.value = a;
}

function handleMoveDown(i: number) {
	if (i === models.value.length - 1) return;
	const a = [...models.value];
	[a[i], a[i + 1]] = [a[i + 1], a[i]];
	models.value = a;
}

async function handleSave() {
	if (!validateName(name.value)) return;
	saving.value = true;
	emit("save", { name: props.forcePrefix + name.value.trim(), models: models.value });
	saving.value = false;
}

const isEdit = !!props.combo;
</script>

<template>
  <Modal
    :is-open="props.isOpen"
    :title="props.title || (isEdit ? 'Edit Combo' : 'Create Combo')"
    @close="emit('close')"
  >
    <div class="flex flex-col gap-3">
      <div>
        <template v-if="props.forcePrefix">
          <label :for="`combo-name-${uid}`" class="text-sm font-medium mb-1 block">Combo Name</label>
          <div class="flex items-stretch">
            <span
              class="inline-flex items-center px-2 rounded-l border border-r-0 border-black/10 dark:border-white/10 bg-black/4 dark:bg-white/4 text-text-muted font-mono text-sm"
              >{{ props.forcePrefix }}</span
            >
            <input
              :id="`combo-name-${uid}`"
              :value="name"
              placeholder="my-combo"
              class="flex-1 min-w-0 rounded-r border border-black/10 dark:border-white/10 bg-white dark:bg-black/20 px-2 py-1.5 font-mono text-sm outline-none focus:border-primary"
              @input="handleNameChange(($event.target as HTMLInputElement).value)"
            />
          </div>
          <p v-if="nameError" class="text-[11px] text-red-500 mt-0.5">{{ nameError }}</p>
        </template>
        <template v-else>
          <Input
            :model-value="name"
            label="Combo Name"
            placeholder="my-combo"
            :error="nameError"
            @update:model-value="handleNameChange"
          />
        </template>
        <p class="text-[10px] text-text-muted mt-0.5">
          <template v-if="props.forcePrefix">Auto-prefixed with "{{ props.forcePrefix }}". </template>Only letters, numbers, -, _ and . allowed
        </p>
      </div>

      <div>
        <div class="text-sm font-medium mb-1.5 block">Models</div>
        <div
          v-if="models.length === 0"
          class="text-center py-4 border border-dashed border-black/10 dark:border-white/10 rounded-lg bg-black/1 dark:bg-white/1"
        >
          <span class="material-symbols-outlined text-text-muted text-xl mb-1">layers</span>
          <p class="text-xs text-text-muted">No models added yet</p>
        </div>
        <!-- Drag handle only: the row body stays click-to-edit. `models` is the
             v-model, so the reordered array is what gets saved; no persistence
             change is needed. -->
        <VueDraggable
          v-else
          v-model="models"
          :animation="150"
          handle=".combo-drag-handle"
          ghost-class="opacity-40"
          class="flex max-h-[55vh] min-w-0 flex-col gap-1 overflow-y-auto sm:max-h-87.5"
        >
          <div
            v-for="(model, index) in models"
            :key="index"
            class="group flex min-w-0 items-center gap-1.5 rounded-md bg-black/2 px-2 py-1 transition-colors hover:bg-black/4 dark:bg-white/2 dark:hover:bg-white/4"
          >
            <button
              type="button"
              class="combo-drag-handle shrink-0 cursor-grab touch-none rounded p-0.5 text-text-muted hover:bg-black/5 hover:text-primary dark:hover:bg-white/5"
              title="Drag to reorder"
            >
              <span class="material-symbols-outlined text-[12px]">drag_indicator</span>
            </button>
            <span class="text-[10px] font-medium text-text-muted w-3 text-center shrink-0">{{ index + 1 }}</span>
            <input
              v-if="editingIndex === index"
              v-model="draft"
              class="min-w-0 flex-1 rounded border border-primary/40 bg-white px-1.5 py-0.5 font-mono text-xs text-text-main outline-none dark:bg-black/20"
              @blur="commitEdit"
              @keydown="handleItemKeyDown"
            />
            <button
              v-else
              type="button"
              class="min-w-0 flex-1 cursor-text truncate rounded px-1.5 py-0.5 font-mono text-xs text-text-main text-left hover:bg-black/5 dark:hover:bg-white/5"
              title="Click to edit"
              @click="startEdit(index)"
            >
              {{ model }}
            </button>
            <div class="flex shrink-0 items-center gap-0.5">
              <button
                type="button"
                :class="`p-0.5 rounded ${index === 0 ? 'text-text-muted/20 cursor-not-allowed' : 'text-text-muted hover:text-primary hover:bg-black/5 dark:hover:bg-white/5'}`"
                :disabled="index === 0"
                title="Move up"
                @click="handleMoveUp(index)"
              >
                <span class="material-symbols-outlined text-[12px]">arrow_upward</span>
              </button>
              <button
                type="button"
                :class="`p-0.5 rounded ${index === models.length - 1 ? 'text-text-muted/20 cursor-not-allowed' : 'text-text-muted hover:text-primary hover:bg-black/5 dark:hover:bg-white/5'}`"
                :disabled="index === models.length - 1"
                title="Move down"
                @click="handleMoveDown(index)"
              >
                <span class="material-symbols-outlined text-[12px]">arrow_downward</span>
              </button>
            </div>
            <button
              type="button"
              class="p-0.5 hover:bg-red-500/10 rounded text-text-muted hover:text-red-500 transition-all"
              title="Remove"
              @click="handleRemoveModel(index)"
            >
              <span class="material-symbols-outlined text-[12px]">close</span>
            </button>
          </div>
        </VueDraggable>
        <button
          type="button"
          class="w-full mt-2 py-2 border border-dashed border-black/10 dark:border-white/10 rounded-lg text-xs text-primary font-medium hover:text-primary hover:border-primary/50 transition-colors flex items-center justify-center gap-1"
          @click="showModelSelect = true"
        >
          <span class="material-symbols-outlined text-[16px]">add</span>
          Add Model
        </button>
      </div>

      <div class="flex flex-col gap-2 pt-1 sm:flex-row">
        <Button variant="ghost" full-width size="sm" @click="emit('close')">Cancel</Button>
        <Button full-width size="sm" :disabled="!name.trim() || !!nameError || saving" @click="handleSave">
          {{ saving ? "Saving..." : isEdit ? "Save" : "Create" }}
        </Button>
      </div>
    </div>
  </Modal>

  <ModelSelectModal
    v-if="showModelSelect"
    :is-open="showModelSelect"
    :active-providers="props.activeProviders"
    :model-aliases="modelAliases"
    title="Add Model to Combo"
    :kind-filter="props.kindFilter"
    :added-model-values="models"
    :close-on-select="false"
    @close="showModelSelect = false"
    @select="handleAddModel"
    @deselect="handleDeselectModel"
  />
</template>
