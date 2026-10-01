<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useRoute } from "vue-router";

const route = useRoute();
const status = ref<"processing" | "success" | "done" | "manual">("processing");
const currentUrl = ref("");

onMounted(() => {
	currentUrl.value = window.location.href;
	const q = route.query;
	const code = (q.code as string) ?? null;
	const token = (q.token as string) ?? null;
	const state = (q.state as string) ?? null;
	const error = (q.error as string) ?? null;
	const errorDescription = (q.error_description as string) ?? null;

	const callbackData = { code, token, state, error, errorDescription, fullUrl: window.location.href };

	// Only the dashboard opener (same origin) or the Codex helper on its fixed
	// loopback port may receive the code/state. Any other origin is hostile.
	const expectedOrigins = [window.location.origin, "http://localhost:1455"];

	if (window.opener) {
		for (const origin of expectedOrigins) {
			try {
				window.opener.postMessage({ type: "oauth_callback", data: callbackData }, origin);
			} catch (e) {
				console.log("postMessage failed:", e);
			}
		}
	}

	try {
		const channel = new BroadcastChannel("oauth_callback");
		channel.postMessage(callbackData);
		channel.close();
	} catch (e) {
		console.log("BroadcastChannel failed:", e);
	}

	try {
		localStorage.setItem("oauth_callback", JSON.stringify({ ...callbackData, timestamp: Date.now() }));
	} catch (e) {
		console.log("localStorage failed:", e);
	}

	if (!(code || token || error)) {
		setTimeout(() => {
			status.value = "manual";
		}, 0);
		return;
	}

	status.value = "success";
	setTimeout(() => {
		window.close();
		setTimeout(() => {
			status.value = "done";
		}, 500);
	}, 1500);
});
</script>

<template>
  <div class="min-h-screen flex items-center justify-center bg-bg">
    <div class="text-center p-8 max-w-md">
      <template v-if="status === 'processing'">
        <div class="size-16 mx-auto mb-4 rounded-full bg-primary/10 flex items-center justify-center">
          <span class="material-symbols-outlined text-3xl text-primary animate-spin">progress_activity</span>
        </div>
        <h1 class="text-xl font-semibold mb-2">Processing...</h1>
        <p class="text-text-muted">Please wait while we complete the authorization.</p>
      </template>

      <template v-else-if="status === 'success' || status === 'done'">
        <div class="size-16 mx-auto mb-4 rounded-full bg-green-100 dark:bg-green-900/30 flex items-center justify-center">
          <span class="material-symbols-outlined text-3xl text-green-600">check_circle</span>
        </div>
        <h1 class="text-xl font-semibold mb-2">Authorization Successful!</h1>
        <p class="text-text-muted">
          {{ status === "success" ? "This window will close automatically..." : "You can close this tab now." }}
        </p>
      </template>

      <template v-else>
        <div class="size-16 mx-auto mb-4 rounded-full bg-yellow-100 dark:bg-yellow-900/30 flex items-center justify-center">
          <span class="material-symbols-outlined text-3xl text-yellow-600">info</span>
        </div>
        <h1 class="text-xl font-semibold mb-2">Copy This URL</h1>
        <p class="text-text-muted mb-4">Please copy the URL from the address bar and paste it in the application.</p>
        <div class="bg-surface border border-border rounded-lg p-3 text-left">
          <code class="text-xs break-all">{{ currentUrl }}</code>
        </div>
      </template>
    </div>
  </div>
</template>
