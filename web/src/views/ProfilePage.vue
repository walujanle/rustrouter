<script setup lang="ts">
import { onMounted, ref } from "vue";
import ConfirmModal from "@/components/ui/ConfirmModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { APP_CONFIG } from "@/constants/config";
import { useTheme } from "@/hooks/useTheme";
import { cn } from "@/utils/cn";

type Status = { type: string; message: string };

const THEME_OPTIONS: Array<"light" | "dark" | "system"> = ["light", "dark", "system"];

const { theme, setTheme } = useTheme();
const shutdownOpen = ref(false);
const isShuttingDown = ref(false);
const settings = ref<Record<string, any>>({ fallbackStrategy: "fill-first" });
const loading = ref(true);
const passwords = ref({ current: "", new: "", confirm: "" });
const passStatus = ref<Status>({ type: "", message: "" });
const passLoading = ref(false);
const dbLoading = ref(false);
const dbStatus = ref<Status>({ type: "", message: "" });
const dbAuth = ref({ open: false, mode: "", password: "" });
const pendingImportRef = ref<File | null>(null);
const importFileRef = ref<HTMLInputElement | null>(null);
const proxyForm = ref({
	outboundProxyEnabled: false,
	outboundProxyUrl: "",
	outboundNoProxy: "",
});
const proxyStatus = ref<Status>({ type: "", message: "" });
const proxyLoading = ref(false);
const proxyTestLoading = ref(false);

const isRemoteHost = ref(false);
const updateStatus = ref<{ latestVersion?: string | null; checkedAt?: string | null }>({});

onMounted(() => {
	isRemoteHost.value = !["localhost", "127.0.0.1", "::1"].includes(window.location.hostname);

	fetch("/api/version")
		.then((res) => res.json())
		.then((data) => {
			updateStatus.value = data;
		})
		.catch(() => {});

	fetch("/api/settings")
		.then((res) => res.json())
		.then((data) => {
			settings.value = data;
			proxyForm.value = {
				outboundProxyEnabled: data?.outboundProxyEnabled === true,
				outboundProxyUrl: data?.outboundProxyUrl || "",
				outboundNoProxy: data?.outboundNoProxy || "",
			};
			loading.value = false;
		})
		.catch((err) => {
			console.error("Failed to fetch settings:", err);
			loading.value = false;
		});
});

async function updateOutboundProxy(e: Event): Promise<void> {
	e.preventDefault();
	if (settings.value.outboundProxyEnabled !== true) return;
	proxyLoading.value = true;
	proxyStatus.value = { type: "", message: "" };

	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				outboundProxyUrl: proxyForm.value.outboundProxyUrl,
				outboundNoProxy: proxyForm.value.outboundNoProxy,
			}),
		});

		const data = await res.json();
		if (res.ok) {
			settings.value = { ...settings.value, ...data };
			proxyStatus.value = { type: "success", message: "Proxy settings applied" };
		} else {
			proxyStatus.value = { type: "error", message: data.error || "Failed to update proxy settings" };
		}
	} catch {
		proxyStatus.value = { type: "error", message: "An error occurred" };
	} finally {
		proxyLoading.value = false;
	}
}

async function testOutboundProxy(): Promise<void> {
	if (settings.value.outboundProxyEnabled !== true) return;

	const proxyUrl = (proxyForm.value.outboundProxyUrl || "").trim();
	if (!proxyUrl) {
		proxyStatus.value = { type: "error", message: "Please enter a Proxy URL to test" };
		return;
	}

	proxyTestLoading.value = true;
	proxyStatus.value = { type: "", message: "" };

	try {
		const res = await fetch("/api/settings/proxy-test", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ proxyUrl }),
		});

		const data = await res.json();
		if (res.ok && data?.ok) {
			proxyStatus.value = {
				type: "success",
				message: `Proxy test OK (${data.status}) in ${data.elapsedMs}ms`,
			};
		} else {
			proxyStatus.value = {
				type: "error",
				message: data?.error || "Proxy test failed",
			};
		}
	} catch {
		proxyStatus.value = { type: "error", message: "An error occurred" };
	} finally {
		proxyTestLoading.value = false;
	}
}

async function updateOutboundProxyEnabled(outboundProxyEnabled: boolean): Promise<void> {
	proxyLoading.value = true;
	proxyStatus.value = { type: "", message: "" };

	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ outboundProxyEnabled }),
		});

		const data = await res.json();
		if (res.ok) {
			settings.value = { ...settings.value, ...data };
			proxyForm.value = {
				...proxyForm.value,
				outboundProxyEnabled: data?.outboundProxyEnabled === true,
			};
			proxyStatus.value = {
				type: "success",
				message: outboundProxyEnabled ? "Proxy enabled" : "Proxy disabled",
			};
		} else {
			proxyStatus.value = { type: "error", message: data.error || "Failed to update proxy settings" };
		}
	} catch {
		proxyStatus.value = { type: "error", message: "An error occurred" };
	} finally {
		proxyLoading.value = false;
	}
}

async function handlePasswordChange(e: Event): Promise<void> {
	e.preventDefault();
	if (passwords.value.new !== passwords.value.confirm) {
		passStatus.value = { type: "error", message: "Passwords do not match" };
		return;
	}

	passLoading.value = true;
	passStatus.value = { type: "", message: "" };

	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				currentPassword: passwords.value.current,
				newPassword: passwords.value.new,
			}),
		});

		const data = await res.json();

		if (res.ok) {
			passStatus.value = { type: "success", message: "Password updated successfully" };
			passwords.value = { current: "", new: "", confirm: "" };
		} else {
			passStatus.value = { type: "error", message: data.error || "Failed to update password" };
		}
	} catch {
		passStatus.value = { type: "error", message: "An error occurred" };
	} finally {
		passLoading.value = false;
	}
}

async function updateFallbackStrategy(strategy: string): Promise<void> {
	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ fallbackStrategy: strategy }),
		});
		if (res.ok) {
			settings.value = { ...settings.value, fallbackStrategy: strategy };
		}
	} catch (err) {
		console.error("Failed to update settings:", err);
	}
}

async function updateComboStrategy(strategy: string): Promise<void> {
	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ comboStrategy: strategy }),
		});
		if (res.ok) {
			settings.value = { ...settings.value, comboStrategy: strategy };
		}
	} catch (err) {
		console.error("Failed to update combo strategy:", err);
	}
}

async function updateStickyLimit(limit: string): Promise<void> {
	const numLimit = Number.parseInt(limit, 10);
	if (Number.isNaN(numLimit) || numLimit < 1) return;

	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ stickyRoundRobinLimit: numLimit }),
		});
		if (res.ok) {
			settings.value = { ...settings.value, stickyRoundRobinLimit: numLimit };
		}
	} catch (err) {
		console.error("Failed to update sticky limit:", err);
	}
}

async function updateComboStickyLimit(limit: string): Promise<void> {
	const numLimit = Number.parseInt(limit, 10);
	if (Number.isNaN(numLimit) || numLimit < 1) return;

	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ comboStickyRoundRobinLimit: numLimit }),
		});
		if (res.ok) {
			settings.value = { ...settings.value, comboStickyRoundRobinLimit: numLimit };
		}
	} catch (err) {
		console.error("Failed to update combo sticky limit:", err);
	}
}

async function updateRequireLogin(requireLogin: boolean): Promise<void> {
	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ requireLogin }),
		});
		if (res.ok) {
			settings.value = { ...settings.value, requireLogin };
		}
	} catch (err) {
		console.error("Failed to update require login:", err);
	}
}

async function updateAutoUpdateCheck(autoUpdateCheck: boolean): Promise<void> {
	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ autoUpdateCheck }),
		});
		if (res.ok) {
			settings.value = { ...settings.value, autoUpdateCheck };
		}
	} catch (err) {
		console.error("Failed to update auto update check:", err);
	}
}

async function reloadSettings(): Promise<void> {
	try {
		const res = await fetch("/api/settings");
		if (!res.ok) return;
		const data = await res.json();
		settings.value = data;
	} catch (err) {
		console.error("Failed to reload settings:", err);
	}
}

async function handleExportDatabase(password: string): Promise<void> {
	dbLoading.value = true;
	dbStatus.value = { type: "", message: "" };
	try {
		const res = await fetch("/api/settings/database", {
			headers: { "x-9r-password": password },
		});
		if (!res.ok) {
			const data = await res.json().catch(() => ({}));
			throw new Error(data.error || "Failed to export database");
		}

		const payload = await res.json();
		const content = JSON.stringify(payload, null, 2);
		const blob = new Blob([content], { type: "application/json" });
		const url = URL.createObjectURL(blob);
		const anchor = document.createElement("a");
		const stamp = new Date().toISOString().replace(/[.:]/g, "-");
		anchor.href = url;
		anchor.download = `rustrouter-backup-${stamp}.json`;
		document.body.appendChild(anchor);
		anchor.click();
		document.body.removeChild(anchor);
		URL.revokeObjectURL(url);

		dbStatus.value = { type: "success", message: "Database backup downloaded" };
	} catch (err) {
		dbStatus.value = { type: "error", message: (err as Error).message || "Failed to export database" };
	} finally {
		dbLoading.value = false;
	}
}

function handleImportDatabase(event: Event): void {
	const file = (event.target as HTMLInputElement).files?.[0];
	if (importFileRef.value) importFileRef.value.value = "";
	if (!file) return;
	pendingImportRef.value = file;
	dbStatus.value = { type: "", message: "" };
	dbAuth.value = { open: true, mode: "import", password: "" };
}

async function runImportDatabase(password: string): Promise<void> {
	const file = pendingImportRef.value;
	if (!file) return;
	dbLoading.value = true;
	try {
		const raw = await file.text();
		const payload = JSON.parse(raw);

		const res = await fetch("/api/settings/database", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ ...payload, password }),
		});

		const data = await res.json().catch(() => ({}));
		if (!res.ok) {
			throw new Error(data.error || "Failed to import database");
		}

		await reloadSettings();
		dbStatus.value = { type: "success", message: "Database imported successfully" };
	} catch (err) {
		dbStatus.value = { type: "error", message: (err as Error).message || "Invalid backup file" };
	} finally {
		pendingImportRef.value = null;
		dbLoading.value = false;
	}
}

// Confirm password modal, then run export or import.
async function handleDbAuthConfirm(): Promise<void> {
	const { mode, password } = dbAuth.value;
	dbAuth.value = { open: false, mode: "", password: "" };
	if (mode === "export") await handleExportDatabase(password);
	else if (mode === "import") await runImportDatabase(password);
}

function onDbAuthKeydown(e: KeyboardEvent): void {
	if (e.key === "Enter" && dbAuth.value.password) handleDbAuthConfirm();
}

async function handleShutdown(): Promise<void> {
	isShuttingDown.value = true;
	try {
		await fetch("/api/version/shutdown", { method: "POST" });
	} catch {
		// Expected to fail as server shuts down; ignore error
	}
	isShuttingDown.value = false;
	shutdownOpen.value = false;
}

async function handleLogout(): Promise<void> {
	try {
		const res = await fetch("/api/auth/logout", { method: "POST" });
		if (res.ok) {
			window.location.assign("/login");
		}
	} catch (err) {
		console.error("Failed to logout:", err);
	}
}
</script>

<template>
  <div class="max-w-2xl mx-auto px-4 sm:px-0">
    <div class="flex flex-col gap-6">
      <!-- Local Mode Info -->
      <Card>
        <div class="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4 mb-4">
          <div class="flex items-center gap-3 sm:gap-4">
            <div class="size-10 sm:size-12 rounded-lg bg-green-500/10 text-green-500 flex items-center justify-center shrink-0">
              <span class="material-symbols-outlined text-xl sm:text-2xl">computer</span>
            </div>
            <div>
              <h2 class="text-lg sm:text-xl font-semibold">Local Mode</h2>
              <p class="text-sm text-text-muted">Running on your machine</p>
            </div>
          </div>
          <div class="inline-flex p-1 rounded-lg bg-black/5 dark:bg-white/5 w-full sm:w-auto">
            <button
              v-for="option in THEME_OPTIONS"
              :key="option"
              type="button"
              :class="cn(
                'flex items-center justify-center gap-1 sm:gap-1.5 px-2 sm:px-3 py-1.5 rounded-md font-medium transition-all flex-1 sm:flex-initial',
                theme === option
                  ? 'bg-white dark:bg-white/10 text-text-main shadow-sm'
                  : 'text-text-muted hover:text-text-main'
              )"
              @click="setTheme(option)"
            >
              <span class="material-symbols-outlined text-[18px]">
                {{ option === "light" ? "light_mode" : option === "dark" ? "dark_mode" : "contrast" }}
              </span>
              <span class="capitalize text-xs sm:text-sm">{{ option }}</span>
            </button>
          </div>
        </div>
        <div class="flex flex-col gap-3 pt-4 border-t border-border">
          <div class="flex flex-col sm:flex-row sm:items-center sm:justify-between p-3 rounded-lg bg-bg border border-border gap-2">
            <div>
              <p class="font-medium text-sm sm:text-base">Database Location</p>
              <p class="text-xs sm:text-sm text-text-muted font-mono break-all">~/.9router/db/data.sqlite</p>
            </div>
          </div>
          <div class="flex flex-col sm:flex-row gap-2">
            <Button
              variant="secondary"
              icon="download"
              :loading="dbLoading"
              class-name="w-full sm:w-auto"
              @click="dbAuth = { open: true, mode: 'export', password: '' }"
            >
              Download Backup
            </Button>
            <Button
              variant="outline"
              icon="upload"
              :disabled="dbLoading"
              class-name="w-full sm:w-auto"
              @click="importFileRef?.click()"
            >
              Import Backup
            </Button>
            <input
              ref="importFileRef"
              type="file"
              accept="application/json,.json"
              class="hidden"
              @change="handleImportDatabase"
            />
          </div>
          <p
            v-if="dbStatus.message"
            :class="['text-sm', dbStatus.type === 'error' ? 'text-red-500' : 'text-green-600 dark:text-green-400']"
          >
            {{ dbStatus.message }}
          </p>
        </div>
      </Card>

      <!-- Security -->
      <Card>
        <div class="flex items-center gap-3 mb-4">
          <div class="p-2 rounded-lg bg-primary/10 text-primary shrink-0">
            <span class="material-symbols-outlined text-[20px]">shield</span>
          </div>
          <h3 class="text-base sm:text-lg font-semibold">Security</h3>
        </div>
        <div class="flex flex-col gap-4">
          <div class="flex items-start sm:items-center justify-between gap-4">
            <div class="flex-1 min-w-0">
              <p class="font-medium text-sm sm:text-base">Require login</p>
              <p class="text-xs sm:text-sm text-text-muted">
                When ON, dashboard requires password. When OFF, access without login.
              </p>
            </div>
            <Toggle
              :model-value="settings.requireLogin === true"
              :disabled="loading"
              @update:model-value="updateRequireLogin(!(settings.requireLogin === true))"
            />
          </div>
          <form
            v-if="settings.requireLogin === true"
            class="flex flex-col gap-4 pt-4 border-t border-border/50"
            @submit.prevent="handlePasswordChange"
          >
            <div v-if="settings.hasPassword" class="flex flex-col gap-2">
              <Input
                label="Current Password"
                type="password"
                placeholder="Enter current password"
                :model-value="passwords.current"
                required
                @update:model-value="passwords = { ...passwords, current: $event }"
              />
            </div>
            <div class="grid grid-cols-1 sm:grid-cols-2 gap-4">
              <div class="flex flex-col gap-2">
                <Input
                  label="New Password"
                  type="password"
                  placeholder="Enter new password"
                  :model-value="passwords.new"
                  required
                  @update:model-value="passwords = { ...passwords, new: $event }"
                />
              </div>
              <div class="flex flex-col gap-2">
                <Input
                  label="Confirm New Password"
                  type="password"
                  placeholder="Confirm new password"
                  :model-value="passwords.confirm"
                  required
                  @update:model-value="passwords = { ...passwords, confirm: $event }"
                />
              </div>
            </div>

            <p
              v-if="passStatus.message"
              :class="['text-xs sm:text-sm', passStatus.type === 'error' ? 'text-red-500' : 'text-green-500']"
            >
              {{ passStatus.message }}
            </p>

            <div class="pt-2">
              <Button type="submit" variant="primary" :loading="passLoading" class-name="w-full sm:w-auto">
                {{ settings.hasPassword ? "Update Password" : "Set Password" }}
              </Button>
            </div>
          </form>
        </div>
      </Card>

      <!-- Routing Preferences -->
      <Card>
        <div class="flex items-center gap-3 mb-4">
          <div class="p-2 rounded-lg bg-blue-500/10 text-blue-500 shrink-0">
            <span class="material-symbols-outlined text-[20px]">route</span>
          </div>
          <h3 class="text-base sm:text-lg font-semibold">Routing Strategy</h3>
        </div>
        <div class="flex flex-col gap-4">
          <div class="flex items-start sm:items-center justify-between gap-4">
            <div class="flex-1 min-w-0">
              <p class="font-medium text-sm sm:text-base">Round Robin</p>
              <p class="text-xs sm:text-sm text-text-muted">
                Cycle through accounts to distribute load
              </p>
            </div>
            <Toggle
              :model-value="settings.fallbackStrategy === 'round-robin'"
              :disabled="loading"
              @update:model-value="updateFallbackStrategy(settings.fallbackStrategy === 'round-robin' ? 'fill-first' : 'round-robin')"
            />
          </div>

          <!-- Sticky Round Robin Limit -->
          <div
            v-if="settings.fallbackStrategy === 'round-robin'"
            class="flex items-start sm:items-center justify-between gap-4 pt-2 border-t border-border/50"
          >
            <div class="flex-1 min-w-0">
              <p class="font-medium text-sm sm:text-base">Sticky Limit</p>
              <p class="text-xs sm:text-sm text-text-muted">
                Calls per account before switching
              </p>
            </div>
            <Input
              type="number"
              min="1"
              max="10"
              :model-value="settings.stickyRoundRobinLimit || 3"
              :disabled="loading"
              class-name="w-16 sm:w-20 text-center shrink-0"
              @update:model-value="updateStickyLimit($event)"
            />
          </div>

          <!-- Combo Round Robin -->
          <div class="flex items-start sm:items-center justify-between gap-4 pt-4 border-t border-border/50">
            <div class="flex-1 min-w-0">
              <p class="font-medium text-sm sm:text-base">Combo Round Robin</p>
              <p class="text-xs sm:text-sm text-text-muted">
                Cycle through providers in combos instead of always starting with first
              </p>
            </div>
            <Toggle
              :model-value="settings.comboStrategy === 'round-robin'"
              :disabled="loading"
              @update:model-value="updateComboStrategy(settings.comboStrategy === 'round-robin' ? 'fallback' : 'round-robin')"
            />
          </div>

          <!-- Combo Sticky Round Robin Limit -->
          <div
            v-if="settings.comboStrategy === 'round-robin'"
            class="flex items-center justify-between pt-2 border-t border-border/50"
          >
            <div>
              <p class="font-medium">Combo Sticky Limit</p>
              <p class="text-sm text-text-muted">
                Calls per combo model before switching
              </p>
            </div>
            <Input
              type="number"
              min="1"
              max="100"
              :model-value="settings.comboStickyRoundRobinLimit || 1"
              :disabled="loading"
              class-name="w-20 text-center"
              @update:model-value="updateComboStickyLimit($event)"
            />
          </div>

          <p class="text-xs text-text-muted italic pt-2 border-t border-border/50">
            {{ settings.fallbackStrategy === "round-robin"
              ? `Currently distributing requests across all available accounts with ${settings.stickyRoundRobinLimit || 3} calls per account.`
              : "Currently using accounts in priority order (Fill First)." }}{{ settings.comboStrategy === "round-robin"
              ? ` Combos rotate after ${settings.comboStickyRoundRobinLimit || 1} call${(settings.comboStickyRoundRobinLimit || 1) === 1 ? "" : "s"} per model.`
              : " Combos always start with their first model." }}
          </p>
        </div>
      </Card>

      <!-- Network -->
      <Card>
        <div class="flex items-center gap-3 mb-4">
          <div class="p-2 rounded-lg bg-purple-500/10 text-purple-500 shrink-0">
            <span class="material-symbols-outlined text-[20px]">wifi</span>
          </div>
          <h3 class="text-base sm:text-lg font-semibold">Network</h3>
        </div>

        <div class="flex flex-col gap-4">
          <div class="flex items-start sm:items-center justify-between gap-4">
            <div class="flex-1 min-w-0">
              <p class="font-medium text-sm sm:text-base">Outbound Proxy</p>
              <p class="text-xs sm:text-sm text-text-muted">Enable proxy for OAuth + provider outbound requests.</p>
            </div>
            <Toggle
              :model-value="settings.outboundProxyEnabled === true"
              :disabled="loading || proxyLoading"
              @update:model-value="updateOutboundProxyEnabled(!(settings.outboundProxyEnabled === true))"
            />
          </div>

          <form
            v-if="settings.outboundProxyEnabled === true"
            class="flex flex-col gap-4 pt-2 border-t border-border/50"
            @submit.prevent="updateOutboundProxy"
          >
            <div class="flex flex-col gap-2">
              <Input
                label="Proxy URL"
                placeholder="http://127.0.0.1:7897"
                :model-value="proxyForm.outboundProxyUrl"
                :disabled="loading || proxyLoading"
                @update:model-value="proxyForm = { ...proxyForm, outboundProxyUrl: $event }"
              />
              <p class="text-xs sm:text-sm text-text-muted">Leave empty to inherit existing env proxy (if any).</p>
            </div>

            <div class="flex flex-col gap-2 pt-2 border-t border-border/50">
              <Input
                label="No Proxy"
                placeholder="localhost,127.0.0.1"
                :model-value="proxyForm.outboundNoProxy"
                :disabled="loading || proxyLoading"
                @update:model-value="proxyForm = { ...proxyForm, outboundNoProxy: $event }"
              />
              <p class="text-xs sm:text-sm text-text-muted">Comma-separated hostnames/domains to bypass the proxy.</p>
            </div>

            <div class="pt-2 border-t border-border/50 flex flex-col sm:flex-row items-stretch sm:items-center gap-2">
              <Button
                type="button"
                variant="secondary"
                :loading="proxyTestLoading"
                :disabled="loading || proxyLoading"
                class-name="w-full sm:w-auto"
                @click="testOutboundProxy"
              >
                Test proxy URL
              </Button>
              <Button type="submit" variant="primary" :loading="proxyLoading" class-name="w-full sm:w-auto">
                Apply
              </Button>
            </div>
          </form>

          <p
            v-if="proxyStatus.message"
            :class="['text-xs sm:text-sm pt-2 border-t border-border/50', proxyStatus.type === 'error' ? 'text-red-500' : 'text-green-500']"
          >
            {{ proxyStatus.message }}
          </p>
        </div>
      </Card>

      <!-- Updates -->
      <Card>
        <div class="flex items-center gap-3 mb-4">
          <div class="p-2 rounded-lg bg-primary/10 text-primary shrink-0">
            <span class="material-symbols-outlined text-[20px]">system_update</span>
          </div>
          <h3 class="text-base sm:text-lg font-semibold">Updates</h3>
        </div>
        <div class="flex flex-col gap-4">
          <div class="flex items-start sm:items-center justify-between gap-4">
            <div class="flex-1 min-w-0">
              <p class="font-medium text-sm sm:text-base">Automatically check for updates</p>
              <p class="text-xs sm:text-sm text-text-muted">
                Checks the GitHub release once a day and at startup. When OFF, no network check runs.
              </p>
            </div>
            <Toggle
              :model-value="settings.autoUpdateCheck === true"
              :disabled="loading"
              @update:model-value="updateAutoUpdateCheck(!(settings.autoUpdateCheck === true))"
            />
          </div>
          <div class="text-xs sm:text-sm text-text-muted pt-4 border-t border-border/50 flex flex-col gap-1">
            <p>Current: v{{ APP_CONFIG.version }}</p>
            <p>Latest: {{ updateStatus.latestVersion ? `v${updateStatus.latestVersion}` : "unknown" }}</p>
            <p v-if="updateStatus.checkedAt">Last checked: {{ new Date(updateStatus.checkedAt).toLocaleString() }}</p>
          </div>
        </div>
      </Card>

      <!-- Account actions -->
      <div class="flex flex-col sm:flex-row gap-2">
        <Button
          variant="outline"
          full-width
          icon="power_settings_new"
          class-name="text-red-500 border-red-200 hover:bg-red-50 hover:border-red-300"
          @click="shutdownOpen = true"
        >
          Shutdown
        </Button>
        <Button
          variant="outline"
          full-width
          icon="logout"
          @click="handleLogout"
        >
          Logout
        </Button>
      </div>

      <!-- App Info -->
      <div class="text-center text-xs sm:text-sm text-text-muted py-4">
        <p>{{ APP_CONFIG.name }} v{{ APP_CONFIG.version }}</p>
        <p class="mt-1">{{ isRemoteHost ? "Remote Mode" : "Local Mode - All data stored on your machine" }}</p>
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

    <Modal
      :is-open="dbAuth.open"
      title="Confirm Password"
      size="sm"
      @close="dbAuth = { open: false, mode: '', password: '' }"
    >
      <p class="text-text-muted mb-3 text-sm">
        Enter your current password to {{ dbAuth.mode === "export" ? "export" : "import" }} the database.
      </p>
      <Input
        type="password"
        :model-value="dbAuth.password"
        placeholder="Current password"
        @update:model-value="dbAuth = { ...dbAuth, password: $event }"
        @keydown="onDbAuthKeydown"
      />
      <template #footer>
        <Button
          variant="ghost"
          :disabled="dbLoading"
          @click="dbAuth = { open: false, mode: '', password: '' }"
        >
          Cancel
        </Button>
        <Button
          variant="primary"
          :loading="dbLoading"
          :disabled="!dbAuth.password"
          @click="handleDbAuthConfirm"
        >
          Confirm
        </Button>
      </template>
    </Modal>
  </div>
</template>
