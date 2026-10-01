<script setup lang="ts">
import Button from "./UiButton.vue";
import Modal from "./UiModal.vue";

const props = withDefaults(
	defineProps<{
		isOpen: boolean;
		title?: string;
		message?: string;
		confirmText?: string;
		cancelText?: string;
		variant?: "primary" | "danger" | "success";
		loading?: boolean;
	}>(),
	{
		title: "Confirm",
		confirmText: "Confirm",
		cancelText: "Cancel",
		variant: "danger",
		loading: false,
	},
);

const emit = defineEmits<{ close: []; confirm: [] }>();
</script>

<template>
  <Modal :is-open="props.isOpen" :title="props.title" size="sm" @close="emit('close')">
    <p class="text-text-muted">{{ props.message }}</p>
    <template #footer>
      <Button variant="ghost" :disabled="props.loading" @click="emit('close')">{{ props.cancelText }}</Button>
      <Button :variant="props.variant" :loading="props.loading" @click="emit('confirm')">{{ props.confirmText }}</Button>
    </template>
  </Modal>
</template>
