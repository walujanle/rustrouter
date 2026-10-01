<script setup lang="ts">
import { computed, reactive, ref } from "vue";
import { RouterLink, useRouter } from "vue-router";
import CardSection from "@/components/ui/CardSection.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import Input from "@/components/ui/UiInput.vue";
import Select from "@/components/ui/UiSelect.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { AUTH_METHODS, useProviders } from "@/constants/providers";

const { AI_PROVIDERS } = useProviders();

const providerOptions = Object.values(AI_PROVIDERS).map((p) => ({
	value: p.id,
	label: p.name,
}));

const authMethodOptions = Object.values(AUTH_METHODS).map((m) => ({
	value: m.id,
	label: m.name,
}));

const router = useRouter();
const loading = ref(false);
const formData = reactive({
	provider: "",
	authMethod: "apikey",
	apiKey: "",
	displayName: "",
	isActive: true,
});
const errors = reactive<Record<string, string | null>>({});

function handleChange(field: string, value: unknown) {
	(formData as Record<string, unknown>)[field] = value;
	if (errors[field]) {
		errors[field] = null;
	}
}

// The credential field serves both keyed auth methods; the backend stores a
// cookie value through the same `apiKey` field.
const needsCredential = computed(
	() => formData.authMethod === "apikey" || formData.authMethod === "cookie",
);

function validate(): boolean {
	const newErrors: Record<string, string> = {};
	if (!formData.provider) newErrors.provider = "Please select a provider";
	if (needsCredential.value && !formData.apiKey) {
		newErrors.apiKey = "API Key is required";
	}
	for (const key of Object.keys(errors)) delete errors[key];
	Object.assign(errors, newErrors);
	return Object.keys(newErrors).length === 0;
}

async function handleSubmit(e: Event) {
	e.preventDefault();
	if (!validate()) return;

	loading.value = true;
	try {
		const response = await fetch("/api/providers", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(formData),
		});

		if (response.ok) {
			router.push("/dashboard/providers");
		} else {
			const data = await response.json();
			errors.submit = data.error || "Failed to create provider";
		}
	} catch {
		errors.submit = "An error occurred. Please try again.";
	} finally {
		loading.value = false;
	}
}

const selectedProvider = computed(() => AI_PROVIDERS[formData.provider]);
</script>

<template>
  <div class="max-w-2xl mx-auto">
    <!-- Header -->
    <div class="mb-8">
      <RouterLink
        to="/dashboard/providers"
        class="inline-flex items-center gap-1 text-sm text-text-muted hover:text-primary transition-colors mb-4"
      >
        <span class="material-symbols-outlined text-lg">arrow_back</span>
        Back to Providers
      </RouterLink>
      <h1 class="text-3xl font-semibold tracking-tight">Add New Provider</h1>
      <p class="text-text-muted mt-2">
        Configure a new AI provider to use with your applications.
      </p>
    </div>

    <!-- Form -->
    <Card>
      <form class="flex flex-col gap-6" @submit="handleSubmit">
        <!-- Provider Selection -->
        <Select
          v-model="formData.provider"
          label="Provider"
          :options="providerOptions"
          placeholder="Select a provider"
          :error="errors.provider || undefined"
          required
        />

        <!-- Provider Info -->
        <CardSection v-if="selectedProvider" class="flex items-center gap-3">
          <div class="size-10 rounded-lg flex items-center justify-center bg-bg border border-border">
            <span
              class="material-symbols-outlined text-xl"
              :style="{ color: selectedProvider.color }"
            >
              {{ selectedProvider.icon }}
            </span>
          </div>
          <div>
            <p class="font-medium">{{ selectedProvider.name }}</p>
            <p class="text-sm text-text-muted">Selected provider</p>
          </div>
        </CardSection>

        <!-- Auth Method -->
        <fieldset class="flex flex-col gap-3">
          <legend class="text-sm font-medium">
            Authentication Method <span class="text-red-500">*</span>
          </legend>
          <div class="flex gap-3">
            <button
              v-for="method in authMethodOptions"
              :key="method.value"
              type="button"
              :class="`flex-1 flex items-center justify-center gap-2 p-4 rounded-lg border transition-all ${
                formData.authMethod === method.value
                  ? 'border-primary bg-primary/5 text-primary'
                  : 'border-border hover:border-primary/50'
              }`"
              @click="handleChange('authMethod', method.value)"
            >
              <span class="material-symbols-outlined">
                {{ method.value === "apikey" ? "key" : method.value === "cookie" ? "cookie" : "lock" }}
              </span>
              <span class="font-medium">{{ method.label }}</span>
            </button>
          </div>
        </fieldset>

        <!-- Credential Input -->
        <Input
          v-if="needsCredential"
          v-model="formData.apiKey"
          :label="formData.authMethod === 'cookie' ? 'Cookie' : 'API Key'"
          type="password"
          :placeholder="formData.authMethod === 'cookie' ? 'Enter your cookie value' : 'Enter your API key'"
          :error="errors.apiKey || undefined"
          :hint="formData.authMethod === 'cookie' ? 'Your cookie value will be encrypted and stored securely.' : 'Your API key will be encrypted and stored securely.'"
          required
        />

        <!-- Display Name -->
        <Input
          v-model="formData.displayName"
          label="Display Name"
          placeholder="e.g., Production API, Dev Environment"
          hint="Optional. A friendly name to identify this configuration."
        />

        <!-- Active Toggle -->
        <Toggle
          v-model="formData.isActive"
          label="Active"
          description="Enable this provider for use in your applications"
        />

        <!-- Error Message -->
        <div
          v-if="errors.submit"
          class="p-4 rounded-lg bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 text-red-600 dark:text-red-400 text-sm"
        >
          {{ errors.submit }}
        </div>

        <!-- Actions -->
        <div class="flex gap-3 pt-4 border-t border-border">
          <RouterLink to="/dashboard/providers" class="flex-1">
            <Button type="button" variant="ghost" full-width>
              Cancel
            </Button>
          </RouterLink>
          <Button type="submit" :loading="loading" full-width class="flex-1">
            Create Provider
          </Button>
        </div>
      </form>
    </Card>
  </div>
</template>
