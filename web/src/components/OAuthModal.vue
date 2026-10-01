<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Input from "@/components/ui/UiInput.vue";
import Modal from "@/components/ui/UiModal.vue";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";

const props = defineProps<{
	isOpen: boolean;
	provider?: string;
	providerInfo?: { name?: string };
	oauthMeta?: Record<string, any>;
}>();

const emit = defineEmits<{ success: []; close: [] }>();

// Providers on the device-code flow (must match the backend's flowType table).
const DEVICE_CODE_PROVIDERS = ["kilocode", "codebuddy-intl", "grok-cli"];

const step = ref<"waiting" | "input" | "success" | "error">("waiting");
const authData = ref<Record<string, any> | null>(null);
const callbackUrl = ref("");
const error = ref<string | null>(null);
const isDeviceCode = ref(false);
const deviceData = ref<Record<string, any> | null>(null);
const polling = ref(false);

let pollingAborted = false;
let opened = false;
let callbackProcessed = false;
// Proxy-flow session ledger: which provider's proxy THIS modal session started,
// and whether its stop was already sent. Every stop-proxy call is gated on this
// so re-renders can never spam it and a close stops the owned proxy once.
const flow = { proxyStarted: false, proxyProvider: null as string | null, stopSent: false };

const { copied, copy } = useCopyToClipboard();

// Client-only values, to avoid a hydration mismatch.
const isLocalhost = ref(false);
const placeholderUrl = ref("/callback?code=...");

onMounted(() => {
	isLocalhost.value =
		window.location.hostname === "localhost" || window.location.hostname === "127.0.0.1";
	placeholderUrl.value = `${window.location.origin}/callback?code=...`;
});

// Exchange tokens
async function exchangeTokens(code: string, state: string | null) {
	if (!authData.value) return;
	try {
		const res = await fetch(`/api/oauth/${props.provider}/exchange`, {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({
				code,
				redirectUri: authData.value.redirectUri,
				codeVerifier: authData.value.codeVerifier,
				state,
				...(props.oauthMeta ? { meta: props.oauthMeta } : {}),
			}),
		});

		const data = await res.json();
		if (!res.ok) throw new Error(data.error);

		step.value = "success";
		emit("success");
	} catch (err) {
		error.value = (err as Error).message;
		step.value = "error";
	}
}

// Poll for device code token
async function startPolling(
	deviceCode: string,
	codeVerifier: string,
	interval: number,
	extraData: Record<string, any> | null,
	deadlineMs?: number,
) {
	pollingAborted = false;
	polling.value = true;
	// Honor the upstream's expires_in when supplied so we don't time out earlier
	// than the device code itself. Default 120s for providers that surface none.
	const startedAt = Date.now();
	// `typeof` narrows `number | undefined` before the comparisons; `Number.isFinite`
	// alone does not, which is why this previously needed non-null assertions.
	const deadline =
		startedAt + (typeof deadlineMs === "number" && Number.isFinite(deadlineMs) && deadlineMs > 0 ? deadlineMs : 120_000);

	while (Date.now() < deadline) {
		if (pollingAborted) {
			console.log("[OAuthModal] Polling aborted");
			polling.value = false;
			return;
		}

		await new Promise((r) => setTimeout(r, interval * 1000));

		if (pollingAborted) {
			console.log("[OAuthModal] Polling aborted after sleep");
			polling.value = false;
			return;
		}

		try {
			const res = await fetch(`/api/oauth/${props.provider}/poll`, {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ deviceCode, codeVerifier, extraData }),
			});

			const data = await res.json();

			if (data.success) {
				pollingAborted = true;
				step.value = "success";
				polling.value = false;
				emit("success");
				return;
			}

			if (data.error === "expired_token" || data.error === "access_denied") {
				throw new Error(data.errorDescription || data.error);
			}

			if (data.error === "slow_down") {
				interval = Math.min(interval + 5, 30);
			}
		} catch (err) {
			error.value = (err as Error).message;
			step.value = "error";
			polling.value = false;
			return;
		}
	}

	error.value = "Authorization timeout";
	step.value = "error";
	polling.value = false;
}

// Stop the proxy owned by THIS modal session, at most once.
function stopOwnedProxy() {
	if (flow.proxyStarted && !flow.stopSent && flow.proxyProvider) {
		flow.stopSent = true;
		fetch(`/api/oauth/${flow.proxyProvider}/stop-proxy`).catch(() => {});
	}
}

async function startOAuthFlow() {
	if (!props.provider) return;
	try {
		error.value = null;

		// Device code flow providers
		if (DEVICE_CODE_PROVIDERS.includes(props.provider)) {
			isDeviceCode.value = true;
			step.value = "waiting";

			const deviceCodeUrl = new URL(`/api/oauth/${props.provider}/device-code`, window.location.origin);
			const res = await fetch(deviceCodeUrl.toString());
			const data = await res.json();
			if (!res.ok) throw new Error(data.error);

			deviceData.value = data;

			// Auto-open verification URL in new tab
			const verifyUrl = data.verification_uri_complete || data.verification_uri;
			if (verifyUrl) window.open(verifyUrl, "_blank", "noopener,noreferrer");

			startPolling(
				data.device_code,
				data.codeVerifier,
				data.interval || 5,
				null,
				// Use the upstream's expires_in if present so we don't time out
				// before the device code itself.
				Number.isFinite(data.expires_in) && data.expires_in > 0 ? data.expires_in * 1000 : undefined,
			);
			return;
		}

		// Authorization code flow — build redirect URI (some providers require fixed ports)
		const appPort = window.location.port || (window.location.protocol === "https:" ? "443" : "80");
		const redirectUri =
			props.provider === "codex" ? "http://localhost:1455/auth/callback" : `http://localhost:${appPort}/callback`;

		// Build authorize URL first to get codeVerifier/state for codex server-side mode
		const authorizeUrl = new URL(`/api/oauth/${props.provider}/authorize`, window.location.origin);
		authorizeUrl.searchParams.set("redirect_uri", redirectUri);
		if (props.oauthMeta) {
			Object.entries(props.oauthMeta).forEach(([k, v]) => {
				if (v) authorizeUrl.searchParams.set(k, v as string);
			});
		}
		const res = await fetch(authorizeUrl.toString());
		const data = await res.json();
		if (!res.ok) throw new Error(data.error);

		// Codex: start proxy with server-side session (auto-exchange) + fallback to channels
		let codexProxyActive = false;
		let codexServerSide = false;
		if (props.provider === "codex") {
			try {
				const proxyUrl = new URL("/api/oauth/codex/start-proxy", window.location.origin);
				proxyUrl.searchParams.set("app_port", appPort);
				proxyUrl.searchParams.set("state", data.state);
				proxyUrl.searchParams.set("code_verifier", data.codeVerifier);
				proxyUrl.searchParams.set("redirect_uri", redirectUri);
				const proxyRes = await fetch(proxyUrl.toString());
				const proxyData = await proxyRes.json();
				codexProxyActive = proxyData.success;
				codexServerSide = !!proxyData.serverSide;
			} catch {
				codexProxyActive = false;
			}
		}

		authData.value = { ...data, redirectUri, codexServerSide };

		// Take ownership of the server-side proxy so close stops it exactly once.
		if (props.provider === "codex" && codexProxyActive) {
			flow.proxyStarted = true;
			flow.proxyProvider = props.provider;
			flow.stopSent = false;
		}

		// Guard: device_code providers return authUrl:null from /authorize. Never window.open(null)
		if (!data.authUrl) {
			if (data.flowType === "device_code") {
				throw new Error(
					`Provider ${props.provider} uses device-code login but is not wired in the OAuth modal device-code list`,
				);
			}
			throw new Error("No authorization URL returned from OAuth provider");
		}

		if (props.provider === "codex" && codexProxyActive) {
			// Proxy active: callback handled server-side (auto-exchange) or via channels (fallback)
			step.value = "waiting";
			if (!window.open(data.authUrl, "oauth_popup", "width=600,height=700")) step.value = "input";
		} else if (!isLocalhost.value || props.provider === "codex") {
			// Non-localhost or proxy failed: manual input mode
			step.value = "input";
			window.open(data.authUrl, "_blank");
		} else {
			// Localhost: open popup and wait for message
			step.value = "waiting";
			if (!window.open(data.authUrl, "oauth_popup", "width=600,height=700")) step.value = "input";
		}
	} catch (err) {
		error.value = (err as Error).message;
		step.value = "error";
	}
}

// Reset state and start OAuth when the modal opens — exactly once per open.
watch(
	() => [props.isOpen, props.provider] as const,
	() => {
		if (!props.isOpen || !props.provider) return;
		if (opened) return;
		opened = true;
		authData.value = null;
		callbackUrl.value = "";
		error.value = null;
		isDeviceCode.value = false;
		deviceData.value = null;
		polling.value = false;
		pollingAborted = false;
		flow.proxyStarted = false;
		flow.proxyProvider = null;
		flow.stopSent = false;
		startOAuthFlow();
	},
	{ immediate: true },
);

// Cleanup when the modal closes: abort polling and stop the proxy THIS session started.
watch(
	() => props.isOpen,
	(open) => {
		if (open) return;
		pollingAborted = true;
		opened = false;
		stopOwnedProxy();
		flow.proxyStarted = false;
		flow.proxyProvider = null;
		flow.stopSent = false;
	},
	{ immediate: true },
);

// Server-side proxy mode (codex fixed-port): poll status until the proxy
// auto-exchanges and saves the connection.
watch(
	authData,
	(data) => {
		const pollProvider = data?.codexServerSide ? "codex" : data?.proxyProvider ? data.proxyProvider : null;
		if (!pollProvider || !data?.state) return;
		if (callbackProcessed) return;
		let cancelled = false;
		const POLL_INTERVAL_MS = 1500;
		const MAX_ATTEMPTS = 200; // ~5 minutes
		let attempts = 0;

		const tick = async () => {
			if (cancelled || callbackProcessed) return;
			attempts += 1;
			try {
				const res = await fetch(
					`/api/oauth/${pollProvider}/poll-status?state=${encodeURIComponent(data.state)}`,
				);
				const payload = await res.json();
				if (cancelled || callbackProcessed) return;
				if (payload.status === "done") {
					callbackProcessed = true;
					step.value = "success";
					emit("success");
					return;
				}
				if (payload.status === "error") {
					callbackProcessed = true;
					error.value = payload.error || "Authentication failed";
					step.value = "error";
					return;
				}
			} catch {
				// Network error, keep polling
			}
			if (attempts >= MAX_ATTEMPTS) {
				callbackProcessed = true;
				error.value = "Authentication timeout";
				step.value = "error";
				return;
			}
			setTimeout(tick, POLL_INTERVAL_MS);
		};
		setTimeout(tick, POLL_INTERVAL_MS);
		return () => {
			cancelled = true;
		};
	},
	{ immediate: true },
);

// Listen for the OAuth callback via multiple methods.
let channel: BroadcastChannel | null = null;

function handleCallback(data: Record<string, any>) {
	if (callbackProcessed) return;

	const { code, token, state, error: callbackError, errorDescription } = data;

	if (callbackError) {
		callbackProcessed = true;
		error.value = errorDescription || callbackError;
		step.value = "error";
		return;
	}

	if (token || code) {
		callbackProcessed = true;
		exchangeTokens(token || code, state);
	}
}

function handleMessage(event: MessageEvent) {
	// Exact origins only: the dashboard's own origin, or the Codex helper on its
	// fixed loopback port. An `includes("localhost")` test would also accept
	// `http://localhost.attacker.com`, which is a different origin entirely.
	const expectedOrigins = [window.location.origin, "http://localhost:1455"];
	if (!expectedOrigins.includes(event.origin)) return;

	if (event.data?.type === "oauth_callback") {
		handleCallback(event.data.data);
	}
}

function handleStorage(event: StorageEvent) {
	if (event.key === "oauth_callback" && event.newValue) {
		try {
			const data = JSON.parse(event.newValue);
			handleCallback(data);
			localStorage.removeItem("oauth_callback");
		} catch {
			console.log("Failed to parse localStorage data");
		}
	}
}

watch(
	authData,
	(data) => {
		if (!data) return;
		callbackProcessed = false; // Reset when authData changes

		window.addEventListener("message", handleMessage);

		try {
			channel = new BroadcastChannel("oauth_callback");
			channel.onmessage = (event) => handleCallback(event.data);
		} catch {
			console.log("BroadcastChannel not supported");
		}

		window.addEventListener("storage", handleStorage);

		// Also check localStorage on mount (in case callback already happened)
		try {
			const stored = localStorage.getItem("oauth_callback");
			if (stored) {
				const parsed = JSON.parse(stored);
				if (parsed.timestamp && Date.now() - parsed.timestamp < 30000) {
					handleCallback(parsed);
				}
				localStorage.removeItem("oauth_callback");
			}
		} catch {
			// localStorage may be unavailable or data malformed — ignore silently
		}
	},
	{ immediate: true },
);

onBeforeUnmount(() => {
	window.removeEventListener("message", handleMessage);
	window.removeEventListener("storage", handleStorage);
	channel?.close();
});

// Handle manual URL input
async function handleManualSubmit() {
	try {
		error.value = null;

		const input = callbackUrl.value.trim();

		// Detect raw JWT access token (starts with eyJ) — skip URL parsing
		if (input.startsWith("eyJ") && input.includes(".")) {
			await exchangeTokens(input, null);
			return;
		}

		const url = new URL(input);
		const code = url.searchParams.get("code");
		const token = url.searchParams.get("token");
		const state = url.searchParams.get("state");
		const errorParam = url.searchParams.get("error");

		if (errorParam) {
			throw new Error(url.searchParams.get("error_description") || errorParam);
		}

		if (!code && !token) {
			throw new Error("No authorization code found in URL");
		}

		await exchangeTokens((token || code) as string, state);
	} catch (err) {
		error.value = (err as Error).message;
		step.value = "error";
	}
}

// Clear session on modal close + cleanup proxy (idempotent).
function handleClose() {
	stopOwnedProxy();
	emit("close");
}

function deviceLoginUrl() {
	return deviceData.value?.verification_uri_complete || deviceData.value?.verification_uri || "";
}

function openDeviceLogin() {
	const url = deviceLoginUrl();
	if (url) window.open(url, "_blank", "noopener,noreferrer");
}

function copyAuthUrl() {
	copy(authData.value?.authUrl || "", "auth_url");
}
</script>

<template>
  <Modal
    v-if="props.provider && props.providerInfo"
    :is-open="props.isOpen"
    :title="`Connect ${props.providerInfo.name}`"
    size="lg"
    @close="handleClose"
  >
    <div class="flex flex-col gap-4">
      <!-- Waiting + Manual Input combined (non-device-code) -->
      <template v-if="(step === 'waiting' || step === 'input') && !isDeviceCode">
        <!-- Option A: Auto via popup -->
        <div class="flex items-center gap-2 px-3 py-2 border border-border rounded-lg bg-sidebar/50">
          <span class="material-symbols-outlined text-base text-primary animate-spin">progress_activity</span>
          <span class="text-sm">Waiting for popup authorization…</span>
        </div>

        <!-- Divider -->
        <div class="flex items-center gap-3 my-1">
          <div class="flex-1 h-px bg-border" />
          <span class="text-xs text-text-muted uppercase tracking-wider">Or paste callback URL manually</span>
          <div class="flex-1 h-px bg-border" />
        </div>

        <!-- Option B: Manual paste -->
        <div class="space-y-4">
          <div>
            <p class="text-sm font-medium mb-2">Step 1: Open this URL in your browser</p>
            <div class="flex gap-2">
              <Input :model-value="authData?.authUrl || ''" read-only class="flex-1 font-mono text-xs" />
              <Button
                variant="secondary"
                :icon="copied === 'auth_url' ? 'check' : 'content_copy'"
                :disabled="!authData?.authUrl"
                @click="copyAuthUrl"
              >
                Copy
              </Button>
            </div>
          </div>

          <div>
            <p class="text-sm font-medium mb-2">Step 2: Paste the callback URL here</p>
            <p class="text-xs text-text-muted mb-2">
              After authorization, copy the full URL from your browser.
            </p>
            <Input v-model="callbackUrl" :placeholder="placeholderUrl" class="font-mono text-xs" />
          </div>
        </div>

        <div class="flex gap-2">
          <Button full-width :disabled="!callbackUrl" @click="handleManualSubmit">Connect</Button>
          <Button variant="ghost" full-width @click="handleClose">Cancel</Button>
        </div>
      </template>

      <!-- Device Code Flow - Waiting -->
      <template v-if="step === 'waiting' && isDeviceCode && deviceData">
        <div class="text-center py-4">
          <p class="text-sm text-text-muted mb-4">Visit the login URL below and authorize:</p>
          <div class="bg-sidebar p-4 rounded-lg mb-4">
            <p class="text-xs text-text-muted mb-1">Login URL</p>
            <div class="flex items-center gap-2">
              <code class="flex-1 text-sm break-all">{{ deviceLoginUrl() }}</code>
              <Button
                size="sm"
                variant="ghost"
                :icon="copied === 'login_url' ? 'check' : 'content_copy'"
                :disabled="!deviceLoginUrl()"
                @click="copy(deviceLoginUrl(), 'login_url')"
              />
              <Button
                size="sm"
                variant="ghost"
                icon="open_in_new"
                :disabled="!deviceLoginUrl()"
                @click="openDeviceLogin"
              >
                Open
              </Button>
            </div>
          </div>
          <div class="bg-primary/10 p-4 rounded-lg">
            <p class="text-xs text-text-muted mb-1">Your Code</p>
            <div class="flex items-center justify-center gap-2">
              <p class="text-2xl font-mono font-bold text-primary">{{ deviceData.user_code }}</p>
              <Button
                size="sm"
                variant="ghost"
                :icon="copied === 'user_code' ? 'check' : 'content_copy'"
                @click="copy(deviceData.user_code, 'user_code')"
              />
            </div>
          </div>
        </div>
        <div v-if="polling" class="flex items-center justify-center gap-2 text-sm text-text-muted">
          <span class="material-symbols-outlined animate-spin">progress_activity</span>
          Waiting for authorization...
        </div>
      </template>

      <!-- Success Step -->
      <div v-if="step === 'success'" class="text-center py-6">
        <div class="size-16 mx-auto mb-4 rounded-full bg-green-100 dark:bg-green-900/30 flex items-center justify-center">
          <span class="material-symbols-outlined text-3xl text-green-600">check_circle</span>
        </div>
        <h3 class="text-lg font-semibold mb-2">Connected Successfully!</h3>
        <p class="text-sm text-text-muted mb-4">
          Your {{ props.providerInfo.name }} account has been connected.
        </p>
        <Button full-width @click="handleClose">Done</Button>
      </div>

      <!-- Error Step -->
      <div v-if="step === 'error'" class="text-center py-6">
        <div class="size-16 mx-auto mb-4 rounded-full bg-red-100 dark:bg-red-900/30 flex items-center justify-center">
          <span class="material-symbols-outlined text-3xl text-red-600">error</span>
        </div>
        <h3 class="text-lg font-semibold mb-2">Connection Failed</h3>
        <p class="text-sm text-red-600 mb-4">{{ error }}</p>
        <div class="flex gap-2">
          <Button variant="secondary" full-width @click="startOAuthFlow">Try Again</Button>
          <Button variant="ghost" full-width @click="handleClose">Cancel</Button>
        </div>
      </div>
    </div>
  </Modal>
</template>
