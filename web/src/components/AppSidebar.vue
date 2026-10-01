<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { RouterLink, useRoute } from "vue-router";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import Button from "@/components/ui/UiButton.vue";
import { APP_CONFIG, UPDATER_CONFIG } from "@/constants/config";
import { useProviders } from "@/constants/providers";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";
import { useSettingsStore } from "@/stores/settings";
import { cn } from "@/utils/cn";

const props = defineProps<{ onClose?: () => void }>();
const emit = defineEmits<{ close: [] }>();

const route = useRoute();
const settingsStore = useSettingsStore();
const { MEDIA_PROVIDER_KINDS } = useProviders();
const { copied, copy } = useCopyToClipboard(2000);

const INSTALL_CMD = UPDATER_CONFIG.installCmdLatest;

const updateInfo = ref<{ latestVersion?: string } | null>(null);
const autoUpdateCheck = ref(true);
const showUpdateModal = ref(false);
const isUpdating = ref(false);
const isDisconnected = ref(false);
const shutdownCountdown = ref(0);
const enableTranslator = ref(false);
let countdownTimer: ReturnType<typeof setInterval> | null = null;
let versionTimer: ReturnType<typeof setTimeout> | null = null;

const navItems = [
	{ href: "/dashboard/endpoint", label: "Endpoint & Key", icon: "api" },
	{ href: "/dashboard/providers", label: "Providers", icon: "dns" },
	{ href: "/dashboard/combos", label: "Combo & Vision Adapter", icon: "layers" },
	{ href: "/dashboard/usage", label: "Usage", icon: "bar_chart" },
	{ href: "/dashboard/quota", label: "Quota Tracker", icon: "data_usage" },
	{ href: "/dashboard/token-saver", label: "Token Saver", icon: "savings" },
	{ href: "/dashboard/cli-tools", label: "CLI Tools", icon: "terminal" },
];

const debugItems = [
	{ href: "/dashboard/console-log", label: "Console Log", icon: "terminal" },
	{ href: "/dashboard/translator", label: "Translator", icon: "translate" },
];

const systemItems = [
	{ href: "/dashboard/proxy-pools", label: "Proxy Pools", icon: "lan" },
	{ href: "/dashboard/skills", label: "Skills", icon: "extension" },
];

// Media kinds with a listing page of their own. webSearch and webFetch share
// the combined /web page instead.
const VISIBLE_MEDIA_KINDS = ["embedding", "systemone"];
const COMBINED_WEB_ITEM = { id: "web", label: "Web Fetch & Search", icon: "travel_explore", href: "/dashboard/media-providers/web" };

const mediaOpen = ref(route.path.startsWith("/dashboard/media-providers"));
watch(
	() => route.path,
	(path) => {
		if (path.startsWith("/dashboard/media-providers")) mediaOpen.value = true;
	},
);
const visibleMediaKinds = computed(() =>
	MEDIA_PROVIDER_KINDS.filter((k) => VISIBLE_MEDIA_KINDS.includes(k.id)),
);
const hasNewMediaKind = computed(() => visibleMediaKinds.value.some((k) => k.isNew));

const visibleDebugItems = computed(() =>
	debugItems.filter((item) => item.href !== "/dashboard/translator" || enableTranslator.value),
);

function isActive(href: string): boolean {
	const path = route.path;
	if (href === "/dashboard/endpoint") return path === "/dashboard" || path.startsWith("/dashboard/endpoint");
	return path.startsWith(href);
}

function close() {
	props.onClose?.();
	emit("close");
}

onMounted(() => {
	settingsStore.fetchSettings().then((data) => {
		if (data?.enableTranslator) enableTranslator.value = true;
		// The banner is an update-check nag, so it respects the same opt-out
		// that stops the daily background loop.
		if (data?.autoUpdateCheck === false) autoUpdateCheck.value = false;
	});

	// Lazy npm-version check after first paint.
	versionTimer = setTimeout(() => {
		fetch("/api/version")
			.then((res) => res.json())
			.then((data) => {
				// `updateAvailable` covers both a newer version and a re-cut
				// release whose binary hash changed under the same version.
				if (data.updateAvailable && autoUpdateCheck.value) updateInfo.value = data;
			})
			.catch(() => {});
	}, 2500);
});

onBeforeUnmount(() => {
	if (versionTimer) clearTimeout(versionTimer);
	if (countdownTimer) clearInterval(countdownTimer);
});

function handleUpdate() {
	showUpdateModal.value = false;
	isUpdating.value = true;
}

async function handleCopyAndShutdown() {
	try {
		await navigator.clipboard.writeText(INSTALL_CMD);
	} catch {
		/* clipboard blocked */
	}
	copy(INSTALL_CMD);
	let remaining = UPDATER_CONFIG.shutdownCountdownSec;
	shutdownCountdown.value = remaining;
	countdownTimer = setInterval(() => {
		remaining -= 1;
		shutdownCountdown.value = remaining;
		if (remaining <= 0) {
			if (countdownTimer) clearInterval(countdownTimer);
			fetch("/api/version/shutdown", { method: "POST" }).catch(() => {});
			isDisconnected.value = true;
		}
	}, 1000);
}

function handleCancelUpdate() {
	isUpdating.value = false;
	shutdownCountdown.value = 0;
}

function reload() {
	globalThis.location.reload();
}
</script>

<template>
  <aside class="flex w-72 flex-col border-r border-border-subtle bg-vibrancy backdrop-blur-xl transition-colors duration-300 min-h-full">
    <div class="flex items-center gap-2 px-6 pt-5 pb-2">
      <div class="w-3 h-3 rounded-full bg-[#FF5F56]" />
      <div class="w-3 h-3 rounded-full bg-[#FFBD2E]" />
      <div class="w-3 h-3 rounded-full bg-[#27C93F]" />
    </div>

    <div class="px-6 py-4 flex flex-col gap-2">
      <RouterLink to="/dashboard" class="flex items-center gap-3">
        <div class="flex items-center justify-center size-9 rounded-[10px] bg-linear-to-br from-brand-500 to-brand-700 shadow-warm">
          <span class="material-symbols-outlined text-white text-[20px]">hub</span>
        </div>
        <div class="flex flex-col">
          <h1 class="text-lg font-semibold tracking-tight text-text-main">{{ APP_CONFIG.name }}</h1>
          <span class="text-xs text-text-muted">v{{ APP_CONFIG.version }}</span>
        </div>
      </RouterLink>
      <div v-if="updateInfo" class="flex flex-col gap-1.5 rounded p-1 -m-1">
        <span class="text-xs font-semibold text-green-600 dark:text-amber-500">
          ↑ New version available: v{{ updateInfo.latestVersion }}
        </span>
        <div class="flex items-center gap-2">
          <button
            type="button"
            class="px-2 py-1 rounded bg-green-600 hover:bg-green-700 dark:bg-amber-500 dark:hover:bg-amber-600 text-white text-[11px] font-semibold transition-colors cursor-pointer"
            @click="showUpdateModal = true"
          >
            Update now
          </button>
          <button
            type="button"
            class="flex-1 text-left hover:opacity-80 transition-opacity cursor-pointer min-w-0"
            title="Copy install command"
            @click="copy(INSTALL_CMD)"
          >
            <code class="block text-[10px] text-green-600/80 dark:text-amber-400/70 font-mono truncate">
              {{ copied ? "✓ copied!" : INSTALL_CMD }}
            </code>
          </button>
        </div>
      </div>
    </div>

    <nav class="flex-1 px-4 py-2 space-y-0.5 overflow-y-auto custom-scrollbar">
      <RouterLink
        v-for="item in navItems"
        :key="item.href"
        :to="item.href"
        :class="cn(
          'flex items-center gap-3 px-3 py-1 rounded-lg transition-all group',
          isActive(item.href) ? 'bg-primary/10 text-primary' : 'text-text-muted hover:bg-surface-2 hover:text-text-main',
        )"
        @click="close"
      >
        <span :class="cn('material-symbols-outlined text-[18px]', isActive(item.href) ? 'fill-1' : 'group-hover:text-primary transition-colors')">
          {{ item.icon }}
        </span>
        <span class="text-[13px] font-medium">{{ item.label }}</span>
      </RouterLink>

      <div class="pt-3 mt-2 space-y-0.5">
        <p class="px-4 text-xs font-semibold text-text-muted/60 uppercase tracking-wider mb-2">System</p>

        <button
          type="button"
          :class="cn(
            'w-full flex items-center gap-3 px-3 py-1 rounded-lg transition-all group',
            route.path.startsWith('/dashboard/media-providers')
              ? 'bg-primary/10 text-primary'
              : 'text-text-muted hover:bg-surface-2 hover:text-text-main',
          )"
          @click="mediaOpen = !mediaOpen"
        >
          <span class="material-symbols-outlined text-[18px]">perm_media</span>
          <span class="text-[13px] font-medium flex-1 text-left">Media Providers</span>
          <span
            v-if="hasNewMediaKind"
            class="text-[10px] font-semibold px-1.5 py-0.5 rounded-[3px] bg-green-500/15 text-green-400"
          >
            NEW
          </span>
          <span
            class="material-symbols-outlined text-[14px] transition-transform"
            :style="{ transform: mediaOpen ? 'rotate(180deg)' : 'rotate(0deg)' }"
          >
            expand_more
          </span>
        </button>
        <div v-if="mediaOpen" class="pl-4">
          <RouterLink
            v-for="kind in visibleMediaKinds"
            :key="kind.id"
            :to="`/dashboard/media-providers/${kind.id}`"
            :class="cn(
              'flex items-center gap-3 px-4 py-1 rounded-lg transition-all group',
              route.path.startsWith(`/dashboard/media-providers/${kind.id}`)
                ? 'bg-primary/10 text-primary'
                : 'text-text-muted hover:bg-surface-2 hover:text-text-main',
            )"
            @click="close"
          >
            <span class="material-symbols-outlined text-[16px]">{{ kind.icon }}</span>
            <span class="text-sm">{{ kind.label }}</span>
            <span
              v-if="kind.isNew"
              class="ml-auto text-[10px] font-semibold px-1.5 py-0.5 rounded-[3px] bg-green-500/15 text-green-400"
            >
              NEW
            </span>
          </RouterLink>
          <RouterLink
            :to="COMBINED_WEB_ITEM.href"
            :class="cn(
              'flex items-center gap-3 px-4 py-1 rounded-lg transition-all group',
              route.path.startsWith(COMBINED_WEB_ITEM.href)
                ? 'bg-primary/10 text-primary'
                : 'text-text-muted hover:bg-surface-2 hover:text-text-main',
            )"
            @click="close"
          >
            <span class="material-symbols-outlined text-[16px]">{{ COMBINED_WEB_ITEM.icon }}</span>
            <span class="text-sm">{{ COMBINED_WEB_ITEM.label }}</span>
          </RouterLink>
        </div>

        <RouterLink
          v-for="item in systemItems"
          :key="item.href"
          :to="item.href"
          :class="cn(
            'flex items-center gap-3 px-3 py-1 rounded-lg transition-all group',
            isActive(item.href) ? 'bg-primary/10 text-primary' : 'text-text-muted hover:bg-surface-2 hover:text-text-main',
          )"
          @click="close"
        >
          <span :class="cn('material-symbols-outlined text-[18px]', isActive(item.href) ? 'fill-1' : 'group-hover:text-primary transition-colors')">
            {{ item.icon }}
          </span>
          <span class="text-[13px] font-medium">{{ item.label }}</span>
        </RouterLink>

        <RouterLink
          v-for="item in visibleDebugItems"
          :key="item.href"
          :to="item.href"
          :class="cn(
            'flex items-center gap-3 px-3 py-1 rounded-lg transition-all group',
            isActive(item.href) ? 'bg-primary/10 text-primary' : 'text-text-muted hover:bg-surface-2 hover:text-text-main',
          )"
          @click="close"
        >
          <span :class="cn('material-symbols-outlined text-[18px]', isActive(item.href) ? 'fill-1' : 'group-hover:text-primary transition-colors')">
            {{ item.icon }}
          </span>
          <span class="text-[13px] font-medium">{{ item.label }}</span>
        </RouterLink>

        <RouterLink
          to="/dashboard/profile"
          :class="cn(
            'flex items-center gap-3 px-3 py-1 rounded-lg transition-all group',
            isActive('/dashboard/profile') ? 'bg-primary/10 text-primary' : 'text-text-muted hover:bg-surface-2 hover:text-text-main',
          )"
          @click="close"
        >
          <span :class="cn('material-symbols-outlined text-[18px]', isActive('/dashboard/profile') ? 'fill-1' : 'group-hover:text-primary transition-colors')">
            settings
          </span>
          <span class="text-[13px] font-medium">Settings</span>
        </RouterLink>
      </div>
    </nav>
  </aside>

  <ConfirmModal
    :is-open="showUpdateModal"
    title="Update RustRouter"
    :message="`Show install command for v${updateInfo?.latestVersion || ''}? You can copy it and shutdown to install manually.`"
    confirm-text="Show Command"
    cancel-text="Cancel"
    variant="primary"
    @close="showUpdateModal = false"
    @confirm="handleUpdate"
  />

  <div v-if="isDisconnected || isUpdating" class="fixed inset-0 z-50 flex items-center justify-center bg-black/80 backdrop-blur-sm p-6">
    <div v-if="isUpdating" class="w-full max-w-lg rounded-xl bg-neutral-900/95 border border-white/10 p-6 text-white">
      <div class="flex items-center gap-3 mb-4">
        <div class="flex items-center justify-center size-11 rounded-full bg-amber-500/20 text-amber-400">
          <span class="material-symbols-outlined text-[24px]">content_copy</span>
        </div>
        <div>
          <h2 class="text-lg font-semibold">Update RustRouter{{ updateInfo?.latestVersion ? ` to v${updateInfo.latestVersion}` : "" }}</h2>
          <p class="text-xs text-white/60">
            {{
              isDisconnected
                ? "Server stopped. Paste the command into a terminal to install."
                : shutdownCountdown > 0
                  ? `Command copied. Server will stop in ${shutdownCountdown}s...`
                  : "Click the button below to copy the install command and shutdown."
            }}
          </p>
        </div>
      </div>

      <p class="text-sm text-white/80 mb-2">Install command:</p>
      <div class="w-full px-3 py-2 rounded bg-white/5 mb-4">
        <code class="text-xs font-mono text-amber-400 break-all">{{ INSTALL_CMD }}</code>
      </div>

      <ol class="text-xs text-white/70 space-y-1 list-decimal list-inside mb-4">
        <li>Click <strong>Copy &amp; Shutdown</strong> below.</li>
        <li>Paste the command into your terminal and press Enter.</li>
        <li>Run <code class="px-1 rounded bg-white/10 text-green-400">rustrouter</code> again after install.</li>
      </ol>

      <Button v-if="isDisconnected" variant="secondary" full-width @click="reload">Reload Page</Button>
      <div v-else class="flex gap-2">
        <Button variant="secondary" :disabled="shutdownCountdown > 0" @click="handleCancelUpdate">Cancel</Button>
        <Button variant="primary" full-width :disabled="shutdownCountdown > 0" @click="handleCopyAndShutdown">
          {{ copied ? "✓ Copied — shutting down..." : shutdownCountdown > 0 ? `Shutting down in ${shutdownCountdown}s` : "Copy & Shutdown" }}
        </Button>
      </div>
    </div>

    <div v-else class="text-center p-8">
      <div class="flex items-center justify-center size-16 rounded-full bg-red-500/20 text-red-500 mx-auto mb-4">
        <span class="material-symbols-outlined text-[32px]">power_off</span>
      </div>
      <h2 class="text-xl font-semibold text-white mb-2">Server Disconnected</h2>
      <p class="text-text-muted mb-6">The proxy server has been stopped.</p>
      <Button variant="secondary" @click="reload">Reload Page</Button>
    </div>
  </div>
</template>
