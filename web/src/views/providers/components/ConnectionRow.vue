<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import Tooltip from "@/components/ui/UiTooltip.vue";
import CooldownTimer from "./CooldownTimer.vue";

type BadgeVariant = "default" | "primary" | "success" | "warning" | "error" | "info";

// Maps a connection's effective status to a badge variant.
function getConnectionStatusVariant(
	isActive: boolean | undefined,
	effectiveStatus: string | undefined,
): BadgeVariant {
	if (isActive === false) return "default";
	if (effectiveStatus === "active" || effectiveStatus === "success") return "success";
	if (
		effectiveStatus === "error" ||
		effectiveStatus === "expired" ||
		effectiveStatus === "unavailable"
	)
		return "error";
	return "default";
}

const props = withDefaults(
	defineProps<{
		connection: Record<string, any>;
		proxyPools?: Array<Record<string, any>>;
		isOAuth: boolean;
		isFirst: boolean;
		isLast: boolean;
		oneByOneStatus?: { state: string; error?: string | null } | null;
		autoPing?: { on: boolean; onToggle: (on: boolean) => void; provider?: string } | null;
	}>(),
	{ proxyPools: () => [], oneByOneStatus: null, autoPing: null },
);

const emit = defineEmits<{
	moveUp: [];
	moveDown: [];
	toggleActive: [isActive: boolean];
	updateProxy: [poolId: string | null];
	edit: [];
	delete: [];
}>();

const showProxyDropdown = ref(false);
const updatingProxy = ref(false);
const proxyDropdownRef = ref<HTMLElement | null>(null);

const proxyPoolMap = computed(
	() => new Map((props.proxyPools || []).map((pool) => [pool.id, pool])),
);
const boundProxyPoolId = computed(
	() => props.connection.providerSpecificData?.proxyPoolId || null,
);
const boundProxyPool = computed(() =>
	boundProxyPoolId.value ? proxyPoolMap.value.get(boundProxyPoolId.value) : null,
);
const hasLegacyProxy = computed(
	() =>
		props.connection.providerSpecificData?.connectionProxyEnabled === true &&
		!!props.connection.providerSpecificData?.connectionProxyUrl,
);
const hasAnyProxy = computed(() => !!boundProxyPoolId.value || hasLegacyProxy.value);

const proxyDisplayText = computed(() => {
	if (boundProxyPool.value) return `Pool: ${boundProxyPool.value.name}`;
	if (boundProxyPoolId.value) return `Pool: ${boundProxyPoolId.value} (inactive/missing)`;
	if (hasLegacyProxy.value)
		return `Legacy: ${props.connection.providerSpecificData?.connectionProxyUrl}`;
	return "";
});

const autoPingTooltip = computed(() =>
	props.autoPing?.provider === "codex"
		? "Auto-starts the next 5h Codex window after reset by sending a tiny gpt-5.5 request. Consumes a small amount of quota."
		: "When your 5h quota runs out, auto-sends a request the moment it resets so a new window starts right away.",
);

const maskedProxyUrl = computed(() => {
	const rawProxyUrl =
		boundProxyPool.value?.proxyUrl ||
		props.connection.providerSpecificData?.connectionProxyUrl;
	if (!rawProxyUrl) return "";
	try {
		const parsed = new URL(rawProxyUrl);
		return `${parsed.protocol}//${parsed.hostname}${parsed.port ? `:${parsed.port}` : ""}`;
	} catch {
		return rawProxyUrl;
	}
});

const noProxyText = computed(
	() =>
		boundProxyPool.value?.noProxy ||
		props.connection.providerSpecificData?.connectionNoProxy ||
		"",
);

const proxyBadgeVariant = computed<BadgeVariant>(() => {
	if (boundProxyPool.value?.isActive === true) return "success";
	if (boundProxyPoolId.value || hasLegacyProxy.value) return "error";
	return "default";
});

const rowAuthType = computed(
	() => props.connection.authType || (props.isOAuth ? "oauth" : "apikey"),
);
const isOAuthConnection = computed(() => rowAuthType.value === "oauth");
const isCookieConnection = computed(() => rowAuthType.value === "cookie");
const authIcon = computed(() =>
	isCookieConnection.value ? "cookie" : isOAuthConnection.value ? "lock" : "key",
);
const authLabel = computed(() =>
	isOAuthConnection.value ? "OAuth" : isCookieConnection.value ? "Cookie" : "API Key",
);
const displayName = computed(
	() =>
		props.connection.name?.trim() ||
		props.connection.email?.trim() ||
		props.connection.displayName?.trim() ||
		(isOAuthConnection.value
			? "OAuth Account"
			: isCookieConnection.value
				? "Cookie Account"
				: "API Key"),
);
const secondaryDisplayName = computed(() => {
	const name = props.connection.name?.trim();
	const email = props.connection.email?.trim();
	const display = props.connection.displayName?.trim();
	if (name && email && name !== email) return email;
	if (name && display && name !== display) return display;
	return null;
});

// The earliest model-lock timestamp; the interval only runs while one exists.
const modelLockUntil = computed(
	() =>
		Object.entries(props.connection)
			.filter(([k]) => k.startsWith("modelLock_"))
			.map(([, v]) => v)
			.filter((v) => !!v)
			.sort()[0] || null,
);

const isCooldown = ref(false);
let cooldownInterval: ReturnType<typeof setInterval> | null = null;

function checkCooldown() {
	const until =
		Object.entries(props.connection)
			.filter(([k]) => k.startsWith("modelLock_"))
			.map(([, v]) => v)
			.filter((v) => v && new Date(v as string).getTime() > Date.now())
			.sort()[0] || null;
	isCooldown.value = !!until;
}

watch(
	modelLockUntil,
	(until) => {
		checkCooldown();
		if (cooldownInterval) {
			clearInterval(cooldownInterval);
			cooldownInterval = null;
		}
		if (until) cooldownInterval = setInterval(checkCooldown, 1000);
	},
	{ immediate: true },
);

const effectiveStatus = computed(() =>
	props.connection.testStatus === "unavailable" && !isCooldown.value
		? "active"
		: props.connection.testStatus,
);

const statusVariant = computed(() =>
	getConnectionStatusVariant(props.connection.isActive, effectiveStatus.value),
);

const oneByOneVariant = computed<BadgeVariant>(() => {
	const state = props.oneByOneStatus?.state;
	if (!state) return "default";
	if (state === "success") return "success";
	if (state === "failed") return "error";
	if (state === "testing") return "primary";
	return "default";
});

const oneByOneLabel = computed(() => {
	const status = props.oneByOneStatus;
	if (!status) return null;
	if (status.state === "queued") return "queued";
	if (status.state === "testing") return "testing";
	if (status.state === "success") return "success";
	if (status.state === "failed")
		return status.error ? `failed: ${status.error}` : "failed";
	return null;
});

function handleProxyOutsideClick(e: MouseEvent) {
	if (proxyDropdownRef.value && !proxyDropdownRef.value.contains(e.target as Node)) {
		showProxyDropdown.value = false;
	}
}

watch(showProxyDropdown, (open) => {
	if (open) document.addEventListener("mousedown", handleProxyOutsideClick);
	else document.removeEventListener("mousedown", handleProxyOutsideClick);
});

onBeforeUnmount(() => {
	document.removeEventListener("mousedown", handleProxyOutsideClick);
	if (cooldownInterval) clearInterval(cooldownInterval);
});

async function handleSelectProxy(poolId: string) {
	updatingProxy.value = true;
	try {
		emit("updateProxy", poolId === "__none__" ? null : poolId);
	} finally {
		updatingProxy.value = false;
		showProxyDropdown.value = false;
	}
}
</script>

<template>
  <div
    :class="`group flex min-w-0 flex-col gap-3 rounded-lg p-2 transition-colors hover:bg-black/2 dark:hover:bg-white/2 sm:flex-row sm:items-center sm:justify-between ${props.connection.isActive === false ? 'opacity-60' : ''}`"
  >
    <div class="flex min-w-0 flex-1 items-start gap-2 sm:items-center sm:gap-3">
      <!-- Priority arrows -->
      <div class="flex shrink-0 flex-col">
        <button
          type="button"
          :disabled="props.isFirst"
          :class="`p-0.5 rounded ${props.isFirst ? 'text-text-muted/30 cursor-not-allowed' : 'hover:bg-sidebar text-text-muted hover:text-primary'}`"
          @click="emit('moveUp')"
        >
          <span class="material-symbols-outlined text-sm">keyboard_arrow_up</span>
        </button>
        <button
          type="button"
          :disabled="props.isLast"
          :class="`p-0.5 rounded ${props.isLast ? 'text-text-muted/30 cursor-not-allowed' : 'hover:bg-sidebar text-text-muted hover:text-primary'}`"
          @click="emit('moveDown')"
        >
          <span class="material-symbols-outlined text-sm">keyboard_arrow_down</span>
        </button>
      </div>
      <span class="material-symbols-outlined shrink-0 text-base text-text-muted">{{ authIcon }}</span>
      <div class="flex-1 min-w-0">
        <p class="text-sm font-medium truncate">{{ displayName }}</p>
        <p v-if="secondaryDisplayName" class="text-xs text-text-muted truncate">{{ secondaryDisplayName }}</p>
        <div class="mt-1 flex min-w-0 flex-wrap items-center gap-1.5 sm:gap-2">
          <Badge :variant="statusVariant" size="sm" dot>
            {{ props.connection.isActive === false ? "disabled" : (effectiveStatus || "Unknown") }}
          </Badge>
          <Badge variant="default" size="sm">{{ authLabel }}</Badge>
          <Badge v-if="hasAnyProxy" :variant="proxyBadgeVariant" size="sm">Proxy</Badge>
          <CooldownTimer
            v-if="isCooldown && props.connection.isActive !== false && modelLockUntil"
            :until="modelLockUntil as string"
          />
          <span
            v-if="props.connection.lastError && props.connection.isActive !== false"
            class="max-w-full truncate text-xs text-red-500 sm:max-w-75"
            :title="props.connection.lastError"
          >{{ props.connection.lastError }}</span>
          <span class="text-xs text-text-muted">#{{ props.connection.priority }}</span>
          <span v-if="props.connection.globalPriority" class="text-xs text-text-muted">Auto: {{ props.connection.globalPriority }}</span>
          <Badge v-if="oneByOneLabel" :variant="oneByOneVariant" size="sm">{{ oneByOneLabel }}</Badge>
        </div>
        <div v-if="hasAnyProxy" class="mt-1 flex items-center gap-2 flex-wrap">
          <span class="max-w-full truncate text-[11px] text-text-muted sm:max-w-105" :title="proxyDisplayText">{{ proxyDisplayText }}</span>
          <code v-if="maskedProxyUrl" class="max-w-full truncate rounded bg-black/5 px-1 py-0.5 font-mono text-[10px] text-text-muted dark:bg-white/5 sm:max-w-65">{{ maskedProxyUrl }}</code>
          <span v-if="noProxyText" class="max-w-full truncate text-[11px] text-text-muted sm:max-w-[320px]" :title="noProxyText">no_proxy: {{ noProxyText }}</span>
        </div>
      </div>
    </div>
    <div class="flex w-full items-center justify-between gap-2 sm:w-auto sm:justify-end">
      <div class="grid flex-1 grid-cols-3 gap-1 sm:flex sm:flex-none">
        <!-- Proxy button with inline dropdown -->
        <div v-if="(props.proxyPools || []).length > 0" ref="proxyDropdownRef" class="relative">
          <button
            type="button"
            :class="`flex w-full flex-col items-center rounded px-2 py-1 transition-colors hover:bg-black/5 dark:hover:bg-white/5 ${hasAnyProxy ? 'text-primary' : 'text-text-muted hover:text-primary'}`"
            :disabled="updatingProxy"
            @click="showProxyDropdown = !showProxyDropdown"
          >
            <span class="material-symbols-outlined text-[18px]">{{ updatingProxy ? "progress_activity" : "lan" }}</span>
            <span class="text-[10px] leading-tight">Proxy</span>
          </button>
          <div
            v-if="showProxyDropdown"
            class="absolute right-0 top-full z-50 mt-1 max-w-[78vw] min-w-40 rounded-lg border border-border bg-bg py-1 shadow-lg"
          >
            <button
              type="button"
              :class="`w-full text-left px-3 py-1.5 text-sm hover:bg-black/5 dark:hover:bg-white/5 ${!boundProxyPoolId ? 'text-primary font-medium' : 'text-text-main'}`"
              @click="handleSelectProxy('__none__')"
            >None</button>
            <button
              type="button"
              v-for="pool in (props.proxyPools || [])"
              :key="pool.id"
              :class="`w-full text-left px-3 py-1.5 text-sm hover:bg-black/5 dark:hover:bg-white/5 ${boundProxyPoolId === pool.id ? 'text-primary font-medium' : 'text-text-main'}`"
              @click="handleSelectProxy(pool.id)"
            >{{ pool.name }}</button>
          </div>
        </div>
        <Tooltip v-if="props.autoPing" :text="autoPingTooltip">
          <button
            type="button"
            :class="`flex w-full flex-col items-center rounded px-2 py-1 transition-colors hover:bg-black/5 dark:hover:bg-white/5 ${props.autoPing.on ? 'text-primary' : 'text-text-muted hover:text-primary'}`"
            @click="props.autoPing.onToggle(!props.autoPing.on)"
          >
            <span class="material-symbols-outlined text-[18px]">bolt</span>
            <span class="text-[10px] leading-tight">Auto-ping</span>
          </button>
        </Tooltip>
        <button
          type="button"
          class="flex flex-col items-center rounded px-2 py-1 text-text-muted hover:bg-black/5 hover:text-primary dark:hover:bg-white/5"
          @click="emit('edit')"
        >
          <span class="material-symbols-outlined text-[18px]">edit</span>
          <span class="text-[10px] leading-tight">Edit</span>
        </button>
        <button
          type="button"
          class="flex flex-col items-center rounded px-2 py-1 text-red-500 hover:bg-red-500/10"
          @click="emit('delete')"
        >
          <span class="material-symbols-outlined text-[18px]">delete</span>
          <span class="text-[10px] leading-tight">Delete</span>
        </button>
      </div>
      <Toggle
        size="sm"
        :model-value="props.connection.isActive ?? true"
        :title="(props.connection.isActive ?? true) ? 'Disable connection' : 'Enable connection'"
        @update:model-value="(v: boolean) => emit('toggleActive', v)"
      />
    </div>
  </div>
</template>
