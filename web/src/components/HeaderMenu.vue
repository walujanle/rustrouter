<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";

import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import { useTheme } from "@/hooks/useTheme";

const emit = defineEmits<{ logout: [] }>();

const { toggleTheme, isDark } = useTheme();

const isOpen = ref(false);
const shutdownOpen = ref(false);
const isShuttingDown = ref(false);
const menuRef = ref<HTMLElement | null>(null);

async function handleShutdown() {
	isShuttingDown.value = true;
	try {
		await fetch("/api/version/shutdown", { method: "POST" });
	} catch {
		// Expected to fail as the server shuts down.
	}
	isShuttingDown.value = false;
	shutdownOpen.value = false;
}

function onDocClick(e: MouseEvent) {
	if (menuRef.value && !menuRef.value.contains(e.target as Node)) isOpen.value = false;
}

onMounted(() => document.addEventListener("mousedown", onDocClick));
onBeforeUnmount(() => document.removeEventListener("mousedown", onDocClick));

function close() {
	isOpen.value = false;
}
</script>

<template>
  <div ref="menuRef" class="relative">
    <button
      type="button"
      class="flex items-center justify-center p-2 rounded-lg text-text-muted hover:text-text-main hover:bg-black/5 dark:hover:bg-white/5 transition-all"
      title="Menu"
      aria-label="Menu"
      aria-haspopup="menu"
      :aria-expanded="isOpen"
      @click="isOpen = !isOpen"
    >
      <span class="material-symbols-outlined">grid_view</span>
    </button>

    <div
      v-if="isOpen"
      class="absolute right-0 top-full mt-2 w-60 bg-surface border border-black/10 dark:border-white/10 rounded-xl shadow-2xl z-50 animate-in fade-in zoom-in-95 duration-150 overflow-hidden py-1"
    >
      <button
        type="button"
        class="flex items-center gap-3 w-full px-4 py-2.5 text-sm transition-colors text-text-main hover:bg-black/5 dark:hover:bg-white/5"
        @click="toggleTheme(); close()"
      >
        <span class="material-symbols-outlined text-[20px] text-text-muted">{{ isDark ? "light_mode" : "dark_mode" }}</span>
        <span class="flex-1 text-left">Theme</span>
      </button>
      <button
        type="button"
        class="flex items-center gap-3 w-full px-4 py-2.5 text-sm transition-colors text-red-500 hover:bg-red-500/10"
        @click="close(); shutdownOpen = true"
      >
        <span class="material-symbols-outlined text-[20px]">power_settings_new</span>
        <span class="flex-1 text-left">Shutdown</span>
      </button>
      <button
        type="button"
        class="flex items-center gap-3 w-full px-4 py-2.5 text-sm transition-colors text-red-500 hover:bg-red-500/10"
        @click="close(); emit('logout')"
      >
        <span class="material-symbols-outlined text-[20px]">logout</span>
        <span class="flex-1 text-left">Logout</span>
      </button>
    </div>
  </div>

  <ConfirmModal
    :is-open="shutdownOpen"
    title="Close Proxy"
    message="Are you sure you want to close the proxy server?"
    confirm-text="Close"
    cancel-text="Cancel"
    variant="danger"
    :loading="isShuttingDown"
    @close="shutdownOpen = false"
    @confirm="handleShutdown"
  />
</template>
