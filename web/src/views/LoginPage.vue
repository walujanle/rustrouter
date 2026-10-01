<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";

import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Input from "@/components/ui/UiInput.vue";

const password = ref("");
const error = ref("");
const resetHint = ref("");
const retryAfter = ref(0);
const loading = ref(false);
const hasPassword = ref<boolean | null>(null);
const mustChange = ref(false);
const newPassword = ref("");
let timer: ReturnType<typeof setInterval> | null = null;

onMounted(async () => {
	const controller = new AbortController();
	const timeoutId = setTimeout(() => controller.abort(), 5000);
	try {
		const res = await fetch(`${window.location.origin}/api/auth/status`, { signal: controller.signal });
		clearTimeout(timeoutId);
		if (res.ok) {
			const data = await res.json();
			if (data.authenticated === true || data.requireLogin === false) {
				window.location.assign("/dashboard");
				return;
			}
			hasPassword.value = !!data.hasPassword;
		} else {
			hasPassword.value = true;
		}
	} catch {
		clearTimeout(timeoutId);
		hasPassword.value = true;
	}
});

onBeforeUnmount(() => {
	if (timer) clearInterval(timer);
});

function startCountdown() {
	if (timer) clearInterval(timer);
	timer = setInterval(() => {
		retryAfter.value = retryAfter.value > 0 ? retryAfter.value - 1 : 0;
		if (retryAfter.value === 0 && timer) clearInterval(timer);
	}, 1000);
}

async function handleLogin(e: Event) {
	e.preventDefault();
	loading.value = true;
	error.value = "";
	resetHint.value = "";
	try {
		const res = await fetch("/api/auth/login", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ password: password.value }),
		});
		if (res.ok) {
			const data = await res.json();
			if (data.mustChangePassword) {
				mustChange.value = true;
				return;
			}
			window.location.assign("/dashboard");
		} else {
			const data = await res.json();
			error.value = data.error || "Invalid password";
			if (data.resetHint) resetHint.value = data.resetHint;
			if (data.retryAfter) {
				retryAfter.value = Number(data.retryAfter);
				startCountdown();
			}
		}
	} catch {
		error.value = "An error occurred. Please try again.";
	} finally {
		loading.value = false;
	}
}

async function handleSetNewPassword(e: Event) {
	e.preventDefault();
	loading.value = true;
	error.value = "";
	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ currentPassword: password.value, newPassword: newPassword.value }),
		});
		if (res.ok) {
			window.location.assign("/dashboard");
		} else {
			const data = await res.json();
			error.value = data.error || "Failed to set password";
		}
	} catch {
		error.value = "An error occurred. Please try again.";
	} finally {
		loading.value = false;
	}
}
</script>

<template>
  <div v-if="hasPassword === null" class="min-h-screen flex items-center justify-center bg-bg p-4">
    <div class="text-center">
      <div class="inline-block animate-spin rounded-full h-8 w-8 border-b-2 border-primary" />
      <p class="text-text-muted mt-4">Loading...</p>
    </div>
  </div>

  <div v-else class="min-h-screen flex items-center justify-center bg-bg p-4 relative overflow-hidden">
    <div class="landing-grid absolute inset-0 pointer-events-none" aria-hidden="true" />
    <div class="relative z-10 w-full max-w-md">
      <div class="text-center mb-8">
        <h1 class="text-3xl font-bold text-primary mb-2">RustRouter</h1>
        <p class="text-text-muted">Enter your password to access the dashboard</p>
      </div>

      <Card>
        <form v-if="mustChange" class="flex flex-col gap-4" @submit="handleSetNewPassword">
          <p class="text-sm text-amber-600 dark:text-amber-400 text-center">
            Set a new password before accessing the dashboard remotely.
          </p>
          <div class="flex flex-col gap-2">
            <Input v-model="newPassword" label="New password" type="password" placeholder="Enter new password" required />
            <p v-if="error" class="text-xs text-red-500">{{ error }}</p>
          </div>
          <Button type="submit" variant="primary" full-width :loading="loading" :disabled="!newPassword">Set password</Button>
        </form>

        <div v-else class="flex flex-col gap-4">
          <form class="flex flex-col gap-4" @submit="handleLogin">
            <div class="flex flex-col gap-2">
              <Input v-model="password" label="Password" type="password" placeholder="Enter password" required />
              <p v-if="error" class="text-xs text-red-500">{{ error }}</p>
              <p v-if="retryAfter > 0" class="text-xs text-amber-600 dark:text-amber-400">
                Locked. Retry in <span class="font-mono">{{ retryAfter }}s</span>.
              </p>
              <p v-if="resetHint" class="text-xs text-text-muted">
                Forgot password? Open <code class="bg-sidebar px-1 rounded">9router</code> CLI on the host → <b>Settings</b> → <b>Reset Password to Default</b>.
              </p>
            </div>

            <Button type="submit" variant="primary" full-width :loading="loading" :disabled="retryAfter > 0">
              {{ retryAfter > 0 ? `Wait ${retryAfter}s` : "Login" }}
            </Button>

            <p class="text-xs text-center text-text-muted mt-2">
              Default password is <code class="bg-sidebar px-1 rounded">123456</code>
            </p>
            <p v-if="hasPassword === false" class="text-xs text-center text-amber-600 dark:text-amber-400">
              Security risk: no password set. You will be asked to set one when logging in remotely.
            </p>
          </form>
        </div>
      </Card>
    </div>
  </div>
</template>
