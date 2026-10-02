<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import EditConnectionModal from "@/components/EditConnectionModal.vue";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import AddApiKeyModal from "./AddApiKeyModal.vue";
import ConnectionRow from "./ConnectionRow.vue";

// Self-contained card: fetches, displays and manages all connections for a provider.
const props = withDefaults(
	defineProps<{
		providerId: string;
		isOAuth?: boolean;
	}>(),
	{ isOAuth: false },
);

const connections = ref<Array<Record<string, any>>>([]);
const proxyPools = ref<Array<Record<string, any>>>([]);
const loading = ref(true);
const showAddModal = ref(false);
const showEditModal = ref(false);
const selectedConnection = ref<Record<string, any> | null>(null);
const providerStrategy = ref<string | null>(null);
const providerStickyLimit = ref("1");
const confirmState = ref<Record<string, any> | null>(null);

const providerId = computed(() => props.providerId);

async function fetch_() {
	try {
		const [connRes, proxyRes, settingsRes] = await Promise.all([
			fetch("/api/providers", { cache: "no-store" }),
			fetch("/api/proxy-pools?isActive=true", { cache: "no-store" }),
			fetch("/api/settings", { cache: "no-store" }),
		]);
		const connData = await connRes.json();
		const proxyData = await proxyRes.json();
		const settingsData = settingsRes.ok ? await settingsRes.json() : {};
		if (connRes.ok)
			connections.value = (connData.connections || []).filter(
				(c: Record<string, any>) => c.provider === providerId.value,
			);
		if (proxyRes.ok) proxyPools.value = proxyData.proxyPools || [];
		const override =
			settingsData.providerStrategies?.[providerId.value] || {};
		providerStrategy.value = override.fallbackStrategy || null;
		providerStickyLimit.value =
			override.stickyRoundRobinLimit != null
				? String(override.stickyRoundRobinLimit)
				: "1";
	} catch (e) {
		console.log("ConnectionsCard fetch error:", e);
	} finally {
		loading.value = false;
	}
}

onMounted(fetch_);
watch(providerId, () => {
	loading.value = true;
	fetch_();
});

async function saveStrategy(strategy: string | null, stickyLimit: string) {
	try {
		const res = await fetch("/api/settings", { cache: "no-store" });
		const data = res.ok ? await res.json() : {};
		const current = data.providerStrategies || {};
		const override: Record<string, any> = {};
		if (strategy) override.fallbackStrategy = strategy;
		if (strategy === "round-robin" && stickyLimit !== "")
			override.stickyRoundRobinLimit = Number(stickyLimit) || 3;
		const updated = { ...current };
		if (Object.keys(override).length === 0) delete updated[providerId.value];
		else updated[providerId.value] = override;
		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ providerStrategies: updated }),
		});
	} catch (e) {
		console.log("saveStrategy error:", e);
	}
}

async function handleSwapPriority(i1: number, i2: number) {
	const next = [...connections.value];
	[next[i1], next[i2]] = [next[i2], next[i1]];
	connections.value = next;
	try {
		await Promise.all([
			fetch(`/api/providers/${next[i1].id}`, {
				method: "PUT",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ priority: i1 }),
			}),
			fetch(`/api/providers/${next[i2].id}`, {
				method: "PUT",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ priority: i2 }),
			}),
		]);
	} catch {
		await fetch_();
	}
}

function handleDelete(id: string) {
	confirmState.value = {
		title: "Delete Connection",
		message: "Delete this connection?",
		onConfirm: async () => {
			confirmState.value = null;
			try {
				const res = await fetch(`/api/providers/${id}`, { method: "DELETE" });
				if (res.ok)
					connections.value = connections.value.filter((c) => c.id !== id);
			} catch (e) {
				console.log("delete error:", e);
			}
		},
	};
}

async function handleToggleActive(id: string, isActive: boolean) {
	try {
		const res = await fetch(`/api/providers/${id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ isActive }),
		});
		if (res.ok)
			connections.value = connections.value.map((c) =>
				c.id === id ? { ...c, isActive } : c,
			);
	} catch (e) {
		console.log("toggle error:", e);
	}
}

async function handleUpdateProxy(connId: string, proxyPoolId: string | null) {
	try {
		const res = await fetch(`/api/providers/${connId}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ proxyPoolId: proxyPoolId || null }),
		});
		if (res.ok)
			connections.value = connections.value.map((c) =>
				c.id === connId
					? {
							...c,
							providerSpecificData: {
								...c.providerSpecificData,
								proxyPoolId: proxyPoolId || null,
							},
						}
					: c,
			);
	} catch (e) {
		console.log("proxy error:", e);
	}
}

async function handleSaveApiKey(formData: Record<string, any>) {
	try {
		const res = await fetch("/api/providers", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ provider: providerId.value, ...formData }),
		});
		if (res.ok) {
			await fetch_();
			showAddModal.value = false;
		}
	} catch (e) {
		console.log("save apikey error:", e);
	}
}

async function handleUpdateConnection(formData: Record<string, any>) {
	try {
		const res = await fetch(`/api/providers/${selectedConnection.value?.id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(formData),
		});
		if (res.ok) {
			await fetch_();
			showEditModal.value = false;
		}
	} catch (e) {
		console.log("update connection error:", e);
	}
}

function openEditModal(conn: Record<string, any>) {
	selectedConnection.value = conn;
	showEditModal.value = true;
}

function onRoundRobinToggle(enabled: boolean) {
	const strategy = enabled ? "round-robin" : null;
	providerStrategy.value = strategy;
	if (enabled && !providerStickyLimit.value) providerStickyLimit.value = "1";
	saveStrategy(
		strategy,
		enabled ? providerStickyLimit.value || "1" : providerStickyLimit.value,
	);
}

function onStickyInput(e: Event) {
	const value = (e.target as HTMLInputElement).value;
	providerStickyLimit.value = value;
	saveStrategy("round-robin", value);
}

function confirmDelete() {
	confirmState.value?.onConfirm?.();
}
</script>

<template>
  <Card v-if="loading">
    <div class="h-20 animate-pulse bg-black/5 rounded-lg" />
  </Card>

  <template v-else>
    <Card>
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between mb-4">
        <h2 class="text-lg font-semibold">Connections</h2>
        <div class="flex flex-wrap items-center gap-2">
          <span class="text-xs text-text-muted font-medium">Round Robin</span>
          <Toggle
            :model-value="providerStrategy === 'round-robin'"
            @update:model-value="onRoundRobinToggle"
          />
          <div
            v-if="providerStrategy === 'round-robin'"
            class="flex flex-wrap items-center gap-1.5"
          >
            <span class="text-xs text-text-muted">Sticky:</span>
            <input
              type="number"
              min="1"
              :value="providerStickyLimit"
              class="w-16 px-2 py-1 text-xs border border-border rounded-md bg-surface focus:outline-none focus:border-primary"
              @input="onStickyInput"
            />
          </div>
        </div>
      </div>

      <div
        v-if="connections.length === 0"
        class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between"
      >
        <p class="text-sm text-text-muted">No connections yet</p>
        <Button size="sm" icon="add" @click="showAddModal = true">Add Connection</Button>
      </div>
      <template v-else>
        <div class="flex flex-col divide-y divide-black/3 dark:divide-white/3 max-h-125 overflow-y-auto pr-1">
          <ConnectionRow
            v-for="(conn, idx) in connections"
            :key="conn.id"
            :connection="conn"
            :proxy-pools="proxyPools"
            :is-o-auth="props.isOAuth"
            :is-first="idx === 0"
            :is-last="idx === connections.length - 1"
            @move-up="handleSwapPriority(idx, idx - 1)"
            @move-down="handleSwapPriority(idx, idx + 1)"
            @toggle-active="(isActive: boolean) => handleToggleActive(conn.id, isActive)"
            @update-proxy="(poolId: string | null) => handleUpdateProxy(conn.id, poolId)"
            @edit="openEditModal(conn)"
            @delete="handleDelete(conn.id)"
          />
        </div>
        <div class="mt-4 flex justify-stretch sm:justify-start">
          <Button size="sm" icon="add" @click="showAddModal = true">Add</Button>
        </div>
      </template>
    </Card>

    <AddApiKeyModal
      :is-open="showAddModal"
      :provider="providerId"
      :proxy-pools="proxyPools"
      @save="handleSaveApiKey"
      @close="showAddModal = false"
    />
    <EditConnectionModal
      :is-open="showEditModal"
      :connection="selectedConnection"
      :proxy-pools="proxyPools"
      @save="handleUpdateConnection"
      @close="showEditModal = false"
    />

    <!-- Confirm Modal -->
    <ConfirmModal
      :is-open="!!confirmState"
      :title="confirmState?.title || 'Confirm'"
      :message="confirmState?.message"
      variant="danger"
      @close="confirmState = null"
      @confirm="confirmDelete"
    />
  </template>
</template>
