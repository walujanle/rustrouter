<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import CardSkeleton from "@/components/ui/CardSkeleton.vue";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { useNotificationStore } from "@/stores/notification";

interface ProxyPool {
	id: string | number;
	name?: string;
	proxyUrl?: string;
	noProxy?: string;
	isActive?: boolean;
	strictProxy?: boolean;
	testStatus?: string;
	lastTestedAt?: string | null;
	lastError?: string | null;
	type?: string;
	boundConnectionCount?: number;
}

interface ProxyPoolForm {
	name: string;
	proxyUrl: string;
	noProxy: string;
	isActive: boolean;
	strictProxy: boolean;
}

interface ConfirmState {
	title: string;
	message: string;
	onConfirm: () => void | Promise<void>;
}

function getStatusVariant(status?: string): "success" | "error" | "default" {
	if (status === "active") return "success";
	if (status === "error") return "error";
	return "default";
}

function formatDateTime(value?: string | null): string {
	if (!value) return "Never";
	const date = new Date(value);
	if (Number.isNaN(date.getTime())) return "Never";
	return date.toLocaleString("en-US");
}

function normalizeFormData(data: Partial<ProxyPool> = {}): ProxyPoolForm {
	return {
		name: data.name || "",
		proxyUrl: data.proxyUrl || "",
		noProxy: data.noProxy || "",
		isActive: data.isActive !== false,
		strictProxy: data.strictProxy === true,
	};
}

const proxyPools = ref<ProxyPool[]>([]);
const loading = ref(true);
const showFormModal = ref(false);
const showBatchImportModal = ref(false);
const showVercelModal = ref(false);
const showCloudflareModal = ref(false);
const showDenoModal = ref(false);
const showRelayMenu = ref(false);
const editingProxyPool = ref<ProxyPool | null>(null);
const formData = ref<ProxyPoolForm>(normalizeFormData());
const batchImportText = ref("");
const vercelForm = ref({ vercelToken: "", projectName: "vercel-relay" });
const cloudflareForm = ref({
	accountId: "",
	apiToken: "",
	projectName: "cloudflare-relay",
});
const denoForm = ref({ denoToken: "", orgDomain: "", projectName: "" });
const saving = ref(false);
const importing = ref(false);
const deploying = ref(false);
const testingId = ref<string | number | null>(null);
const selectedIds = ref<Array<string | number>>([]);
const healthChecking = ref(false);
const healthProgress = ref({ current: 0, total: 0 });
const bulkBusy = ref(false);
const confirmState = ref<ConfirmState | null>(null);
const relayMenuRef = ref<HTMLElement | null>(null);
const notify = useNotificationStore();

function handleClickOutside(e: MouseEvent) {
	if (relayMenuRef.value && !relayMenuRef.value.contains(e.target as Node)) {
		showRelayMenu.value = false;
	}
}

watch(showRelayMenu, (open) => {
	document.removeEventListener("mousedown", handleClickOutside);
	if (open) document.addEventListener("mousedown", handleClickOutside);
});

onBeforeUnmount(() => {
	document.removeEventListener("mousedown", handleClickOutside);
});

async function fetchProxyPools() {
	try {
		const res = await fetch("/api/proxy-pools?includeUsage=true", {
			cache: "no-store",
		});
		const data = await res.json();
		if (res.ok) {
			proxyPools.value = data.proxyPools || [];
		}
	} catch (error) {
		console.log("Error fetching proxy pools:", error);
	} finally {
		loading.value = false;
	}
}

onMounted(() => {
	fetchProxyPools();
});

function resetForm() {
	editingProxyPool.value = null;
	formData.value = normalizeFormData();
}

function openCreateModal() {
	resetForm();
	showFormModal.value = true;
}

function openEditModal(proxyPool: ProxyPool) {
	editingProxyPool.value = proxyPool;
	formData.value = normalizeFormData(proxyPool);
	showFormModal.value = true;
}

function closeFormModal() {
	showFormModal.value = false;
	resetForm();
}

async function handleSave() {
	const payload = {
		name: formData.value.name.trim(),
		proxyUrl: formData.value.proxyUrl.trim(),
		noProxy: formData.value.noProxy.trim(),
		isActive: formData.value.isActive === true,
		strictProxy: formData.value.strictProxy === true,
	};

	if (!payload.name || !payload.proxyUrl) return;

	saving.value = true;
	// Capture before closeFormModal() resets it, so the toast reports the right verb.
	const isEdit = !!editingProxyPool.value;
	try {
		const res = await fetch(
			isEdit
				? `/api/proxy-pools/${editingProxyPool.value?.id}`
				: "/api/proxy-pools",
			{
				method: isEdit ? "PUT" : "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify(payload),
			},
		);

		if (res.ok) {
			await fetchProxyPools();
			closeFormModal();
			notify.success(isEdit ? "Proxy pool updated" : "Proxy pool created");
		} else {
			const data = await res.json();
			notify.error(data.error || "Failed to save proxy pool");
		}
	} catch (error) {
		console.log("Error saving proxy pool:", error);
	} finally {
		saving.value = false;
	}
}

function handleDelete(proxyPool: ProxyPool) {
	confirmState.value = {
		title: "Delete Proxy Pool",
		message: `Delete proxy pool "${proxyPool.name}"?`,
		onConfirm: async () => {
			confirmState.value = null;
			try {
				const res = await fetch(`/api/proxy-pools/${proxyPool.id}`, {
					method: "DELETE",
				});
				if (res.ok) {
					proxyPools.value = proxyPools.value.filter(
						(item) => item.id !== proxyPool.id,
					);
					notify.success("Proxy pool deleted");
					return;
				}

				const data = await res.json();
				if (res.status === 409) {
					notify.warning(
						`Cannot delete: ${data.boundConnectionCount || 0} connection(s) are still using this pool.`,
					);
				} else {
					notify.error(data.error || "Failed to delete proxy pool");
				}
			} catch (error) {
				console.log("Error deleting proxy pool:", error);
				notify.error("Failed to delete proxy pool");
			}
		},
	};
}

async function handleTest(proxyPoolId: string | number) {
	testingId.value = proxyPoolId;
	try {
		const res = await fetch(`/api/proxy-pools/${proxyPoolId}/test`, {
			method: "POST",
		});
		const data = await res.json();

		if (!res.ok) {
			notify.error(data.error || "Failed to test proxy");
			return;
		}

		await fetchProxyPools();
		notify.success(data.ok ? "Proxy test passed" : "Proxy test failed");
	} catch (error) {
		console.log("Error testing proxy pool:", error);
		notify.error("Failed to test proxy");
	} finally {
		testingId.value = null;
	}
}

async function handleToggleActive(pool: ProxyPool) {
	const next = !pool.isActive;
	proxyPools.value = proxyPools.value.map((p) =>
		p.id === pool.id ? { ...p, isActive: next } : p,
	);
	try {
		const res = await fetch(`/api/proxy-pools/${pool.id}`, {
			method: "PUT",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ isActive: next }),
		});
		if (!res.ok) {
			proxyPools.value = proxyPools.value.map((p) =>
				p.id === pool.id ? { ...p, isActive: pool.isActive } : p,
			);
			notify.error("Failed to update active state");
		}
	} catch (error) {
		console.log("Error toggling active:", error);
		proxyPools.value = proxyPools.value.map((p) =>
			p.id === pool.id ? { ...p, isActive: pool.isActive } : p,
		);
	}
}

const allSelected = computed(
	() =>
		proxyPools.value.length > 0 &&
		selectedIds.value.length === proxyPools.value.length,
);

function toggleSelect(id: string | number) {
	selectedIds.value = selectedIds.value.includes(id)
		? selectedIds.value.filter((x) => x !== id)
		: [...selectedIds.value, id];
}

function toggleSelectAll() {
	selectedIds.value = allSelected.value
		? []
		: proxyPools.value.map((p) => p.id);
}

function clearSelection() {
	selectedIds.value = [];
}

async function bulkSetActive(isActive: boolean) {
	const targets =
		selectedIds.value.length > 0
			? selectedIds.value
			: proxyPools.value.map((p) => p.id);
	if (targets.length === 0) return;
	bulkBusy.value = true;
	try {
		let ok = 0;
		let failed = 0;
		for (const id of targets) {
			try {
				const res = await fetch(`/api/proxy-pools/${id}`, {
					method: "PUT",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify({ isActive }),
				});
				if (res.ok) ok += 1;
				else failed += 1;
			} catch {
				failed += 1;
			}
		}
		await fetchProxyPools();
		notify.success(
			`${isActive ? "Activated" : "Deactivated"} ${ok}${failed ? `, failed ${failed}` : ""}`,
		);
	} finally {
		bulkBusy.value = false;
	}
}

function bulkDelete() {
	if (selectedIds.value.length === 0) return;
	confirmState.value = {
		title: "Delete Proxy Pools",
		message: `Delete ${selectedIds.value.length} proxy pool(s)?`,
		onConfirm: async () => {
			confirmState.value = null;
			bulkBusy.value = true;
			try {
				let ok = 0;
				let blocked = 0;
				let failed = 0;
				for (const id of selectedIds.value) {
					try {
						const res = await fetch(`/api/proxy-pools/${id}`, {
							method: "DELETE",
						});
						if (res.ok) ok += 1;
						else if (res.status === 409) blocked += 1;
						else failed += 1;
					} catch {
						failed += 1;
					}
				}
				await fetchProxyPools();
				clearSelection();
				notify.success(
					`Deleted ${ok}${blocked ? `, ${blocked} bound` : ""}${failed ? `, ${failed} failed` : ""}`,
				);
			} finally {
				bulkBusy.value = false;
			}
		},
	};
}

async function handleHealthCheck() {
	const targets =
		selectedIds.value.length > 0
			? proxyPools.value.filter((p) => selectedIds.value.includes(p.id))
			: proxyPools.value;
	if (targets.length === 0) return;
	healthChecking.value = true;
	healthProgress.value = { current: 0, total: targets.length };
	let alive = 0;
	const deadIds: Array<string | number> = [];
	let done = 0;
	const CONCURRENCY = 10;
	const queue = [...targets];

	const worker = async () => {
		while (queue.length > 0) {
			const pool = queue.shift();
			if (!pool) break;
			try {
				const res = await fetch(`/api/proxy-pools/${pool.id}/test`, {
					method: "POST",
				});
				const data = await res.json();
				if (res.ok && data.ok) alive += 1;
				else deadIds.push(pool.id);
			} catch {
				deadIds.push(pool.id);
			} finally {
				done += 1;
				healthProgress.value = { current: done, total: targets.length };
			}
		}
	};

	await Promise.all(
		Array.from({ length: Math.min(CONCURRENCY, targets.length) }, worker),
	);
	await fetchProxyPools();
	healthChecking.value = false;
	healthProgress.value = { current: 0, total: 0 };

	if (deadIds.length > 0) {
		confirmState.value = {
			title: "Disable Dead Proxies",
			message: `Alive: ${alive}, Dead: ${deadIds.length}.\n\nDisable ${deadIds.length} dead proxies?`,
			onConfirm: async () => {
				confirmState.value = null;
				bulkBusy.value = true;
				try {
					for (const id of deadIds) {
						try {
							await fetch(`/api/proxy-pools/${id}`, {
								method: "PUT",
								headers: { "Content-Type": "application/json" },
								body: JSON.stringify({ isActive: false }),
							});
						} catch {}
					}
					await fetchProxyPools();
					notify.success(`Disabled ${deadIds.length} dead proxies`);
				} finally {
					bulkBusy.value = false;
				}
			},
		};
	} else {
		notify.success(`Health check done. Alive: ${alive}, Dead: ${deadIds.length}`);
	}
}

// Cleanup selectedIds when pools change
watch(proxyPools, () => {
	selectedIds.value = selectedIds.value.filter((id) =>
		proxyPools.value.some((p) => p.id === id),
	);
});

function openBatchImportModal() {
	batchImportText.value = "";
	showBatchImportModal.value = true;
}

function closeBatchImportModal() {
	if (importing.value) return;
	showBatchImportModal.value = false;
}

function openVercelModal() {
	vercelForm.value = { vercelToken: "", projectName: "vercel-relay" };
	showVercelModal.value = true;
}

function closeVercelModal() {
	if (deploying.value) return;
	showVercelModal.value = false;
}

function openCloudflareModal() {
	cloudflareForm.value = {
		accountId: "",
		apiToken: "",
		projectName: "cloudflare-relay",
	};
	showCloudflareModal.value = true;
}

function closeCloudflareModal() {
	if (deploying.value) return;
	showCloudflareModal.value = false;
}

function openDenoModal() {
	denoForm.value = { denoToken: "", orgDomain: "", projectName: "" };
	showDenoModal.value = true;
}

function closeDenoModal() {
	if (deploying.value) return;
	showDenoModal.value = false;
}

async function handleVercelDeploy() {
	if (!vercelForm.value.vercelToken.trim()) return;
	deploying.value = true;
	try {
		const res = await fetch("/api/proxy-pools/vercel-deploy", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(vercelForm.value),
		});
		const data = await res.json();
		if (res.ok) {
			await fetchProxyPools();
			closeVercelModal();
			notify.success(`Deployed: ${data.deployUrl}`);
		} else {
			notify.error(data.error || "Deploy failed");
		}
	} catch (error) {
		console.log("Error deploying Vercel relay:", error);
		notify.error("Deploy failed");
	} finally {
		deploying.value = false;
	}
}

async function handleCloudflareDeploy() {
	if (!cloudflareForm.value.accountId.trim() || !cloudflareForm.value.apiToken.trim())
		return;
	deploying.value = true;
	try {
		const res = await fetch("/api/proxy-pools/cloudflare-deploy", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(cloudflareForm.value),
		});
		const data = await res.json();
		if (res.ok) {
			await fetchProxyPools();
			closeCloudflareModal();
			notify.success(`Deployed: ${data.deployUrl}`);
		} else {
			notify.error(data.error || "Deploy failed");
		}
	} catch (error) {
		console.log("Error deploying Cloudflare relay:", error);
		notify.error("Deploy failed");
	} finally {
		deploying.value = false;
	}
}

async function handleDenoDeploy() {
	if (!denoForm.value.denoToken.trim()) return;
	deploying.value = true;
	try {
		const res = await fetch("/api/proxy-pools/deno-deploy", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(denoForm.value),
		});
		const data = await res.json();
		if (res.ok) {
			await fetchProxyPools();
			closeDenoModal();
			notify.success(`Deployed: ${data.deployUrl}`);
		} else {
			notify.error(data.error || "Deploy failed");
		}
	} catch (error) {
		console.log("Error deploying Deno relay:", error);
		notify.error("Deploy failed");
	} finally {
		deploying.value = false;
	}
}

function parseProxyLine(line: string): { proxyUrl: string; name: string } | null {
	const trimmed = line.trim();
	if (!trimmed) return null;

	if (trimmed.includes("://")) {
		const parsed = new URL(trimmed);
		const hostLabel = parsed.port
			? `${parsed.hostname}:${parsed.port}`
			: parsed.hostname;
		return {
			proxyUrl: parsed.toString(),
			name: `Imported ${hostLabel}`,
		};
	}

	const parts = trimmed.split(":");
	if (parts.length === 4) {
		const [host, port, username, password] = parts;
		if (!host || !port || !username || !password) {
			throw new Error("Invalid host:port:user:pass format");
		}

		const proxyUrl = `http://${encodeURIComponent(username)}:${encodeURIComponent(password)}@${host}:${port}`;
		const parsed = new URL(proxyUrl);
		return {
			proxyUrl: parsed.toString(),
			name: `Imported ${host}:${port}`,
		};
	}

	throw new Error("Unsupported format");
}

async function handleBatchImport() {
	const lines = batchImportText.value
		.split(/\r?\n/)
		.map((line) => line.trim())
		.filter(Boolean);

	if (lines.length === 0) {
		notify.warning("Please paste at least one proxy line.");
		return;
	}

	const parsedEntries: Array<{
		proxyUrl: string;
		name: string;
		lineNumber: number;
	}> = [];
	const invalidLines: string[] = [];

	lines.forEach((line, index) => {
		try {
			const parsed = parseProxyLine(line);
			if (parsed) {
				parsedEntries.push({
					...parsed,
					lineNumber: index + 1,
				});
			}
		} catch (error) {
			invalidLines.push(`Line ${index + 1}: ${(error as Error).message}`);
		}
	});

	if (invalidLines.length > 0) {
		notify.error(`Invalid proxy format:\n${invalidLines.join("\n")}`);
		return;
	}

	importing.value = true;
	try {
		const existingKeys = new Set(
			proxyPools.value.map(
				(pool) =>
					`${(pool.proxyUrl || "").trim()}|||${(pool.noProxy || "").trim()}`,
			),
		);

		let created = 0;
		let skipped = 0;
		let failed = 0;

		for (const entry of parsedEntries) {
			const dedupeKey = `${entry.proxyUrl}|||`;
			if (existingKeys.has(dedupeKey)) {
				skipped += 1;
				continue;
			}

			const res = await fetch("/api/proxy-pools", {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({
					name: entry.name,
					proxyUrl: entry.proxyUrl,
					noProxy: "",
					isActive: true,
				}),
			});

			if (res.ok) {
				created += 1;
				existingKeys.add(dedupeKey);
			} else {
				failed += 1;
			}
		}

		await fetchProxyPools();
		showBatchImportModal.value = false;
		notify.success(
			`Batch import completed: Created ${created}, Skipped ${skipped}, Failed ${failed}`,
		);
	} catch (error) {
		console.log("Error batch importing proxies:", error);
		notify.error("Batch import failed");
	} finally {
		importing.value = false;
	}
}

const activeCount = computed(
	() => proxyPools.value.filter((pool) => pool.isActive === true).length,
);

function handleConfirm() {
	confirmState.value?.onConfirm();
}
</script>

<template>
  <div
    v-if="loading"
    class="mx-auto flex w-full max-w-5xl flex-col gap-4 px-1 sm:gap-6 sm:px-0"
  >
    <CardSkeleton />
    <CardSkeleton />
  </div>

  <div
    v-else
    class="mx-auto flex w-full max-w-5xl flex-col gap-4 px-1 sm:gap-6 sm:px-0"
  >
    <div class="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
      <div class="min-w-0">
        <h1 class="text-xl font-semibold sm:text-2xl">Proxy Pools</h1>
      </div>

      <div class="grid grid-cols-1 gap-2 sm:flex sm:items-center">
        <div ref="relayMenuRef" class="relative">
          <Button
            size="sm"
            variant="secondary"
            icon="rocket_launch"
            @click="showRelayMenu = !showRelayMenu"
          >
            Deploy Relay
            <span class="material-symbols-outlined ml-1 text-[18px]">
              {{ showRelayMenu ? "expand_less" : "expand_more" }}
            </span>
          </Button>

          <div
            v-if="showRelayMenu"
            class="absolute left-0 top-full z-50 mt-1 w-48 rounded-xl border border-black/10 bg-white p-1 shadow-xl dark:border-white/10 dark:bg-zinc-900 sm:left-auto sm:right-0"
          >
            <button
              type="button"
              class="flex w-full items-center gap-2 rounded-lg px-3 py-2 text-sm text-text-main transition-colors hover:bg-black/5 dark:hover:bg-white/5"
              @click="openCloudflareModal(); showRelayMenu = false"
            >
              <span class="material-symbols-outlined text-[20px] text-orange-500">cloud</span>
              Cloudflare Relay
            </button>
            <button
              type="button"
              class="flex w-full items-center gap-2 rounded-lg px-3 py-2 text-sm text-text-main transition-colors hover:bg-black/5 dark:hover:bg-white/5"
              @click="openVercelModal(); showRelayMenu = false"
            >
              <span class="material-symbols-outlined text-[20px] text-blue-500">cloud_upload</span>
              Vercel Relay
            </button>
            <button
              type="button"
              class="flex w-full items-center gap-2 rounded-lg px-3 py-2 text-sm text-text-main transition-colors hover:bg-black/5 dark:hover:bg-white/5"
              @click="openDenoModal(); showRelayMenu = false"
            >
              <span class="material-symbols-outlined text-[20px] text-green-500">terminal</span>
              Deno Relay
            </button>
          </div>
        </div>

        <Button size="sm" variant="secondary" icon="upload" @click="openBatchImportModal">
          Batch Import
        </Button>
        <Button size="sm" icon="add" @click="openCreateModal">Add Proxy Pool</Button>
      </div>
    </div>

    <Card>
      <div class="mb-4 flex flex-wrap items-center gap-2">
        <label
          v-if="proxyPools.length > 0"
          class="flex items-center gap-1.5 text-xs text-text-muted cursor-pointer"
        >
          <input
            type="checkbox"
            :checked="allSelected"
            class="size-4 rounded border-black/20 dark:border-white/20"
            @change="toggleSelectAll"
          />
          {{ allSelected ? "Unselect all" : "Select all" }}
        </label>
        <Badge variant="default">Total: {{ proxyPools.length }}</Badge>
        <Badge variant="success">Active: {{ activeCount }}</Badge>
      </div>

      <div
        v-if="selectedIds.length > 0 || healthChecking"
        class="mb-4 flex flex-wrap items-center gap-2 rounded-lg border border-primary/30 bg-primary/5 px-3 py-2"
      >
        <span class="material-symbols-outlined text-[18px] text-primary">checklist</span>
        <span class="text-xs font-medium text-primary">
          {{ selectedIds.length > 0 ? `${selectedIds.length} selected` : "All pools" }}
        </span>
        <div class="ml-auto flex flex-wrap items-center gap-2">
          <Button
            size="sm"
            :icon="healthChecking ? 'progress_activity' : 'health_and_safety'"
            :disabled="healthChecking || bulkBusy || proxyPools.length === 0"
            @click="handleHealthCheck"
          >
            {{ healthChecking ? `Checking ${healthProgress.current}/${healthProgress.total}` : "Health Check" }}
          </Button>
          <template v-if="selectedIds.length > 0">
            <Button
              size="sm"
              variant="secondary"
              icon="toggle_on"
              :disabled="bulkBusy || healthChecking"
              @click="bulkSetActive(true)"
            >
              Activate
            </Button>
            <Button
              size="sm"
              variant="secondary"
              icon="toggle_off"
              :disabled="bulkBusy || healthChecking"
              @click="bulkSetActive(false)"
            >
              Deactivate
            </Button>
            <Button
              size="sm"
              variant="secondary"
              icon="delete"
              :disabled="bulkBusy || healthChecking"
              @click="bulkDelete"
            >
              Delete
            </Button>
            <Button
              size="sm"
              variant="ghost"
              :disabled="bulkBusy || healthChecking"
              @click="clearSelection"
            >
              Clear
            </Button>
          </template>
        </div>
      </div>

      <div v-if="proxyPools.length === 0" class="text-center py-10">
        <p class="text-text-main font-medium mb-1">No proxy pool entries yet</p>
        <p class="text-sm text-text-muted mb-4">
          Create a proxy pool entry, then assign it to connections.
        </p>
        <Button icon="add" @click="openCreateModal">Add Proxy Pool</Button>
      </div>
      <div v-else class="flex flex-col divide-y divide-black/4 dark:divide-white/5">
        <div
          v-for="pool in proxyPools"
          :key="pool.id"
          class="flex flex-col gap-3 py-3 sm:flex-row sm:items-center sm:justify-between"
        >
          <div class="flex items-start gap-3 min-w-0 flex-1">
            <input
              type="checkbox"
              :checked="selectedIds.includes(pool.id)"
              :aria-label="`Select ${pool.name}`"
              class="mt-1 size-4 shrink-0 rounded border-black/20 dark:border-white/20"
              @change="toggleSelect(pool.id)"
            />
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-2 flex-wrap">
                <p class="min-w-0 max-w-full truncate text-sm font-medium sm:max-w-[18rem]">{{ pool.name }}</p>
                <Badge :variant="getStatusVariant(pool.testStatus)" size="sm" dot>
                  {{ pool.testStatus || "unknown" }}
                </Badge>
                <Badge :variant="pool.isActive ? 'success' : 'default'" size="sm">
                  {{ pool.isActive ? "active" : "inactive" }}
                </Badge>
                <Badge v-if="pool.type === 'vercel'" variant="default" size="sm">vercel relay</Badge>
                <Badge v-if="pool.type === 'cloudflare'" variant="default" size="sm">cloudflare relay</Badge>
                <Badge variant="default" size="sm">
                  {{ pool.boundConnectionCount || 0 }} bound
                </Badge>
              </div>
              <p class="text-xs text-text-muted truncate mt-1">{{ pool.proxyUrl }}</p>
              <p v-if="pool.noProxy" class="text-xs text-text-muted truncate">
                No proxy: {{ pool.noProxy }}
              </p>
              <p class="text-[11px] text-text-muted mt-1">
                Last tested: {{ formatDateTime(pool.lastTestedAt) }}
                {{ pool.lastError ? ` · ${pool.lastError}` : "" }}
              </p>
            </div>
          </div>

          <div class="flex items-center justify-end gap-1">
            <Toggle
              size="sm"
              :model-value="pool.isActive === true"
              :title="pool.isActive ? 'Disable' : 'Enable'"
              @update:model-value="handleToggleActive(pool)"
            />
            <button
              type="button"
              class="p-2 rounded hover:bg-black/5 dark:hover:bg-white/5 text-text-muted hover:text-primary"
              title="Test proxy"
              :disabled="testingId === pool.id"
              @click="handleTest(pool.id)"
            >
              <span
                class="material-symbols-outlined text-[18px]"
                :style="testingId === pool.id ? { animation: 'spin 1s linear infinite' } : undefined"
              >
                {{ testingId === pool.id ? "progress_activity" : "science" }}
              </span>
            </button>
            <button
              type="button"
              class="p-2 rounded hover:bg-black/5 dark:hover:bg-white/5 text-text-muted hover:text-primary"
              title="Edit"
              @click="openEditModal(pool)"
            >
              <span class="material-symbols-outlined text-[18px]">edit</span>
            </button>
            <button
              type="button"
              class="p-2 rounded hover:bg-red-500/10 text-red-500"
              title="Delete"
              @click="handleDelete(pool)"
            >
              <span class="material-symbols-outlined text-[18px]">delete</span>
            </button>
          </div>
        </div>
      </div>
    </Card>

    <Modal
      :is-open="showBatchImportModal"
      title="Batch Import Proxies"
      @close="closeBatchImportModal"
    >
      <div class="flex flex-col gap-4">
        <div>
          <label for="batch-import-text" class="text-sm font-medium text-text-main mb-1 block">Paste Proxy List (One per line)</label>
          <textarea
            id="batch-import-text"
            :value="batchImportText"
            placeholder="http://user:pass@127.0.0.1:7897
127.0.0.1:7897:user:pass"
            class="w-full min-h-45 py-2 px-3 text-sm text-text-main bg-white dark:bg-white/5 border border-black/10 dark:border-white/10 rounded-md focus:ring-1 focus:ring-primary/30 focus:border-primary/50 focus:outline-none transition-all"
            @input="batchImportText = ($event.target as HTMLTextAreaElement).value"
          />
          <p class="text-xs text-text-muted mt-1">
            Supported formats: protocol://user:pass@host:port, host:port:user:pass
          </p>
        </div>

        <div class="grid grid-cols-1 gap-2 sm:grid-cols-2">
          <Button full-width :disabled="!batchImportText.trim() || importing" @click="handleBatchImport">
            {{ importing ? "Importing..." : "Import" }}
          </Button>
          <Button full-width variant="ghost" :disabled="importing" @click="closeBatchImportModal">
            Cancel
          </Button>
        </div>
      </div>
    </Modal>

    <Modal :is-open="showVercelModal" title="Deploy Vercel Relay" @close="closeVercelModal">
      <div class="flex flex-col gap-4">
        <div class="rounded-lg bg-blue-500/5 border border-blue-500/10 p-3 flex flex-col gap-1.5">
          <p class="text-sm text-text-main font-medium">What is Vercel Relay?</p>
          <p class="text-xs text-text-muted">
            Deploys an edge relay function to Vercel. All AI provider requests will be forwarded through Vercel&apos;s edge network, masking your real IP from providers.
          </p>
          <ul class="text-xs text-text-muted list-disc pl-4 space-y-0.5">
            <li>Your IP is replaced by Vercel&apos;s dynamic edge IPs (hundreds of IPs across 20+ global regions)</li>
            <li>Vercel serves millions of apps — providers can&apos;t block Vercel IPs without affecting legitimate traffic</li>
            <li>Free tier: 100GB bandwidth/month, 500K edge invocations</li>
            <li>Deploy multiple relays on different accounts for more IP diversity</li>
          </ul>
        </div>
        <Input
          v-model="vercelForm.vercelToken"
          label="Vercel API Token"
          placeholder="your-vercel-api-token"
          type="password"
        >
          <template #hint>
            Token is used once for deployment and not stored. <a href="https://vercel.com/account/tokens" target="_blank" rel="noopener noreferrer" class="text-primary hover:underline">Get token →</a>
          </template>
        </Input>
        <Input
          v-model="vercelForm.projectName"
          label="Project Name"
          placeholder="my-relay"
          hint="Unique name for your Vercel project. Leave empty for auto-generated name."
        />
        <div class="grid grid-cols-1 gap-2 sm:grid-cols-2">
          <Button
            full-width
            :disabled="!vercelForm.vercelToken.trim() || deploying"
            @click="handleVercelDeploy"
          >
            {{ deploying ? "Deploying... (may take ~1 min)" : "Deploy" }}
          </Button>
          <Button full-width variant="ghost" :disabled="deploying" @click="closeVercelModal">
            Cancel
          </Button>
        </div>
      </div>
    </Modal>

    <Modal
      :is-open="showCloudflareModal"
      title="Deploy Cloudflare Relay"
      @close="closeCloudflareModal"
    >
      <div class="flex flex-col gap-4">
        <div class="rounded-lg bg-orange-500/5 border border-orange-500/10 p-3 flex flex-col gap-1.5">
          <p class="text-sm text-text-main font-medium">What is Cloudflare Relay?</p>
          <p class="text-xs text-text-muted">
            Deploys a Cloudflare Worker as a proxy relay. All AI provider requests will be forwarded through Cloudflare&apos;s global edge network.
          </p>
          <ul class="text-xs text-text-muted list-disc pl-4 space-y-0.5">
            <li>High performance global routing and IP masking via Cloudflare Workers</li>
            <li>Free tier: 100,000 requests per day</li>
            <li>Requires Cloudflare Account ID and a Workers API Token (Edit Workers permission)</li>
          </ul>
          <div class="mt-2 pt-2 border-t border-orange-500/10 text-xs text-text-muted">
            <p class="font-medium text-text-main mb-1">How to generate your API Token:</p>
            <ol class="list-decimal pl-4 space-y-0.5">
              <li>Go to <b>My Profile</b> → <b>API Tokens</b> → <b>Create Token</b></li>
              <li>Scroll down to <b>Custom Token</b> and click <b>Get started</b></li>
              <li>Under <b>Permissions</b>: Account | Workers Scripts | Edit</li>
              <li>Under <b>Account Resources</b>: Include | Account | <i>Your Account Name</i></li>
              <li>Click <b>Continue to summary</b> → <b>Create Token</b></li>
            </ol>
          </div>
        </div>
        <Input
          v-model="cloudflareForm.accountId"
          label="Account ID"
          placeholder="your-cloudflare-account-id"
          hint="Found on the right side of the Cloudflare dashboard overview page."
        />
        <Input
          v-model="cloudflareForm.apiToken"
          label="API Token"
          placeholder="your-cloudflare-api-token"
          type="password"
        >
          <template #hint>
            Requires "Workers Scripts: Edit" permission. <a href="https://dash.cloudflare.com/profile/api-tokens" target="_blank" rel="noopener noreferrer" class="text-primary hover:underline">Get token →</a>
          </template>
        </Input>
        <Input
          v-model="cloudflareForm.projectName"
          label="Worker Name"
          placeholder="my-relay"
          hint="Unique name for your Cloudflare Worker. Leave empty for auto-generated name."
        />
        <div class="grid grid-cols-1 gap-2 sm:grid-cols-2">
          <Button
            full-width
            :disabled="!cloudflareForm.accountId.trim() || !cloudflareForm.apiToken.trim() || deploying"
            @click="handleCloudflareDeploy"
          >
            {{ deploying ? "Deploying..." : "Deploy Worker" }}
          </Button>
          <Button full-width variant="ghost" :disabled="deploying" @click="closeCloudflareModal">
            Cancel
          </Button>
        </div>
      </div>
    </Modal>

    <Modal :is-open="showDenoModal" title="Deploy Deno Relay" @close="closeDenoModal">
      <div class="flex flex-col gap-4">
        <div class="rounded-lg bg-black/5 dark:bg-white/5 border border-black/10 dark:border-white/10 p-3 flex flex-col gap-1.5">
          <p class="text-sm text-text-main font-medium">What is Deno Relay?</p>
          <p class="text-xs text-text-muted">
            Deploys a relay worker to Deno Deploy&apos;s global edge network. All AI provider requests are forwarded through Deno&apos;s edge, masking your real IP.
          </p>
          <ul class="text-xs text-text-muted list-disc pl-4 space-y-0.5">
            <li>Deno Deploy v2 runs on a high-performance global edge network</li>
            <li>Free tier: 1M requests & 100GiB outbound traffic per month</li>
            <li>No per-request CPU time limits (unlike Vercel/Cloudflare)</li>
            <li>Support up to 20 active apps & 50 custom domains</li>
            <li>Deploy multiple relays for maximum IP diversity</li>
          </ul>
          <div class="mt-2 pt-2 border-t border-black/10 dark:border-white/10 text-xs text-text-muted">
            <p class="font-medium text-text-main mb-1">How to generate API token:</p>
            <ol class="list-decimal pl-4 space-y-0.5">
              <li>Go to <b>console.deno.com</b></li>
              <li>Select your <b>Organization</b> → <b>Settings</b> → <b>Organization Tokens</b></li>
              <li>Create a <b>Organization Token</b> (prefix <b>ddo_</b>)</li>
            </ol>
          </div>
        </div>
        <Input
          v-model="denoForm.denoToken"
          label="Deno Deploy API Token"
          placeholder="ddo_xxxxxxxxxxxxxxxx"
          type="password"
          hint="Token is used once for deployment, not stored. Found in Organization Settings."
        />
        <Input
          v-model="denoForm.orgDomain"
          label="Organization Domain"
          placeholder="your-org.deno.net"
          hint="Organization's default domain. Your relay URL will be in the format: https://my-relay.your-org.deno.net"
        />
        <Input
          v-model="denoForm.projectName"
          label="App Name"
          placeholder="deno-relay"
          hint="Unique app name. Leave empty for auto-generated name."
        />
        <div class="grid grid-cols-1 gap-2 sm:grid-cols-2">
          <Button
            full-width
            :disabled="!denoForm.denoToken.trim() || !denoForm.orgDomain.trim() || deploying"
            @click="handleDenoDeploy"
          >
            {{ deploying ? "Deploying..." : "Deploy Relay" }}
          </Button>
          <Button full-width variant="ghost" :disabled="deploying" @click="closeDenoModal">
            Cancel
          </Button>
        </div>
      </div>
    </Modal>

    <Modal
      :is-open="showFormModal"
      :title="editingProxyPool ? 'Edit Proxy Pool' : 'Add Proxy Pool'"
      @close="closeFormModal"
    >
      <div class="flex flex-col gap-4">
        <Input
          v-model="formData.name"
          label="Name"
          placeholder="Office Proxy"
        />
        <Input
          v-model="formData.proxyUrl"
          label="Proxy URL"
          placeholder="http://127.0.0.1:7897"
        />
        <Input
          v-model="formData.noProxy"
          label="No Proxy"
          placeholder="localhost,127.0.0.1,.internal"
          hint="Comma-separated hosts/domains to bypass proxy"
        />

        <div class="flex flex-col gap-3 rounded-lg border border-border/50 p-3 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <p class="font-medium text-sm">Active</p>
            <p class="text-xs text-text-muted">Inactive pools are ignored by runtime resolution.</p>
          </div>
          <Toggle v-model="formData.isActive" :disabled="saving" />
        </div>

        <div class="flex flex-col gap-3 rounded-lg border border-border/50 p-3 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <p class="font-medium text-sm">Strict Proxy</p>
            <p class="text-xs text-text-muted">Fail request if proxy is unreachable instead of falling back to direct.</p>
          </div>
          <Toggle v-model="formData.strictProxy" :disabled="saving" />
        </div>

        <div class="grid grid-cols-1 gap-2 sm:grid-cols-2">
          <Button
            full-width
            :disabled="!formData.name.trim() || !formData.proxyUrl.trim() || saving"
            @click="handleSave"
          >
            {{ saving ? "Saving..." : "Save" }}
          </Button>
          <Button full-width variant="ghost" :disabled="saving" @click="closeFormModal">
            Cancel
          </Button>
        </div>
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
  </div>
</template>
