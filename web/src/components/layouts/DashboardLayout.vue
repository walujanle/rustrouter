<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { RouterView, useRoute } from "vue-router";

import Header from "@/components/AppHeader.vue";
import Sidebar from "@/components/AppSidebar.vue";
import { useNotificationStore } from "@/stores/notification";

const route = useRoute();
const notifications = useNotificationStore();
const sidebarOpen = ref(false);

function getToastStyle(type: string) {
	if (type === "success")
		return { wrapper: "border-green-500/30 bg-green-500/10 text-green-600 dark:text-green-400", icon: "check_circle" };
	if (type === "error")
		return { wrapper: "border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-400", icon: "error" };
	if (type === "warning")
		return { wrapper: "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400", icon: "warning" };
	return { wrapper: "border-blue-500/30 bg-blue-500/10 text-blue-600 dark:text-blue-400", icon: "info" };
}

const isBasicChat = computed(() => route.name === "basic-chat");

let idleId: number | null = null;
let idleTimer: ReturnType<typeof setTimeout> | null = null;

// Preload heavy usage charts while the browser is idle.
onMounted(() => {
	const preload = () => {
		import("@/components/UsageStats.vue").catch(() => {});
		import("@/views/usage/components/UsageChart.vue").catch(() => {});
		import("@/views/usage/components/ProviderBarChart.vue").catch(() => {});
		import("@/views/usage/components/TopModelsChart.vue").catch(() => {});
	};
	if ("requestIdleCallback" in window) {
		idleId = window.requestIdleCallback(preload, { timeout: 4000 });
	} else {
		idleTimer = setTimeout(preload, 2500);
	}
});

onBeforeUnmount(() => {
	if (idleId !== null) window.cancelIdleCallback(idleId);
	if (idleTimer) clearTimeout(idleTimer);
});
</script>

<template>
  <div class="flex h-screen w-full overflow-hidden bg-bg">
    <div class="fixed top-4 right-4 z-80 flex w-[min(92vw,380px)] flex-col gap-2">
      <div
        v-for="n in notifications.notifications"
        :key="n.id"
        :class="`rounded-lg border px-3 py-2 shadow-lg backdrop-blur-sm ${getToastStyle(n.type).wrapper}`"
      >
        <div class="flex items-start gap-2">
          <span class="material-symbols-outlined text-[18px] leading-5">{{ getToastStyle(n.type).icon }}</span>
          <div class="min-w-0 flex-1">
            <p v-if="n.title" class="text-xs font-semibold mb-0.5">{{ n.title }}</p>
            <p class="text-xs whitespace-pre-wrap break-words">{{ n.message }}</p>
          </div>
          <button
            v-if="n.dismissible"
            type="button"
            class="text-current/70 hover:text-current"
            aria-label="Dismiss notification"
            @click="notifications.removeNotification(n.id)"
          >
            <span class="material-symbols-outlined text-[16px]">close</span>
          </button>
        </div>
      </div>
    </div>

    <button
      v-if="sidebarOpen"
      type="button"
      class="fixed inset-0 z-40 bg-black/20 lg:hidden"
      aria-label="Close sidebar"
      @click="sidebarOpen = false"
    />

    <div class="hidden lg:flex">
      <Sidebar />
    </div>

    <div
      :class="`fixed inset-y-0 left-0 z-50 transform lg:hidden transition-transform duration-300 ease-in-out ${sidebarOpen ? 'translate-x-0' : '-translate-x-full'}`"
    >
      <Sidebar @close="sidebarOpen = false" />
    </div>

    <main class="flex flex-col flex-1 h-full min-w-0 relative transition-colors duration-300 isolate">
      <div class="landing-grid absolute inset-0 pointer-events-none -z-10" aria-hidden="true" />
      <Header :key="route.fullPath" @menu-click="sidebarOpen = true" />
      <div
        :class="`flex-1 overflow-y-auto custom-scrollbar ${isBasicChat ? 'flex flex-col overflow-hidden' : 'p-6 lg:p-10'}`"
      >
        <div :class="isBasicChat ? 'flex-1 w-full h-full flex flex-col' : 'max-w-7xl mx-auto'">
          <RouterView />
        </div>
      </div>
    </main>
  </div>
</template>
