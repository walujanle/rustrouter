<script setup lang="ts">
import { onMounted, ref } from "vue";

import PricingModal from "@/components/PricingModal.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";

type PricingData = Record<string, Record<string, Record<string, number>>>;

const showModal = ref(false);
const currentPricing = ref<PricingData | null>(null);
const loading = ref(true);
const loadError = ref<string | null>(null);

onMounted(() => {
	loadPricing();
});

async function loadPricing() {
	loading.value = true;
	loadError.value = null;
	try {
		const response = await fetch("/api/pricing");
		if (response.ok) {
			const data = await response.json();
			currentPricing.value = data;
		} else {
			loadError.value = `Failed to load pricing (HTTP ${response.status}).`;
		}
	} catch (error) {
		console.error("Failed to load pricing:", error);
		loadError.value = "Failed to load pricing. Check the server and try again.";
	} finally {
		loading.value = false;
	}
}

function handlePricingUpdated() {
	loadPricing();
}

// Count total models with pricing
function getModelCount(): number {
	if (!currentPricing.value) return 0;
	let count = 0;
	for (const provider in currentPricing.value) {
		count += Object.keys(currentPricing.value[provider]).length;
	}
	return count;
}

// Get providers list
function getProviders(): string[] {
	if (!currentPricing.value) return [];
	return Object.keys(currentPricing.value).sort();
}
</script>

<template>
  <div class="max-w-6xl mx-auto p-6 space-y-6">
    <!-- Header -->
    <div class="flex items-center justify-between">
      <div>
        <h1 class="text-3xl font-bold">Pricing Settings</h1>
        <p class="text-text-muted mt-1">
          Configure pricing rates for cost tracking and calculations
        </p>
      </div>
      <button
        type="button"
        class="px-4 py-2 bg-primary text-white rounded hover:bg-primary/90 transition-colors"
        @click="showModal = true"
      >
        Edit Pricing
      </button>
    </div>

    <!-- Load failure -->
    <div
      v-if="loadError"
      class="flex items-center justify-between gap-4 rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"
    >
      <span>{{ loadError }}</span>
      <Button size="sm" variant="outline" @click="loadPricing">Retry</Button>
    </div>

    <!-- Quick Stats -->
    <div class="grid grid-cols-1 md:grid-cols-3 gap-4">
      <Card class-name="p-4">
        <div class="text-text-muted text-sm uppercase font-semibold">
          Total Models
        </div>
        <div class="text-2xl font-bold mt-1">
          {{ loading ? "..." : getModelCount() }}
        </div>
      </Card>
      <Card class-name="p-4">
        <div class="text-text-muted text-sm uppercase font-semibold">
          Providers
        </div>
        <div class="text-2xl font-bold mt-1">
          {{ loading ? "..." : getProviders().length }}
        </div>
      </Card>
      <Card class-name="p-4">
        <div class="text-text-muted text-sm uppercase font-semibold">
          Status
        </div>
        <div
          class="text-2xl font-bold mt-1"
          :class="loadError ? 'text-danger' : 'text-success'"
        >
          {{ loading ? "..." : loadError ? "Unavailable" : "Active" }}
        </div>
      </Card>
    </div>

    <!-- Info Section -->
    <Card class-name="p-6">
      <h2 class="text-xl font-semibold mb-4">How Pricing Works</h2>
      <div class="space-y-3 text-sm text-text-muted">
        <p>
          <strong>Cost Calculation:</strong> Costs are calculated based on token usage and pricing rates.
          Each request's cost is determined by: (input_tokens × input_rate) + (output_tokens × output_rate) + (cached_tokens × cached_rate)
        </p>
        <p>
          <strong>Pricing Format:</strong> All rates are in <strong>dollars per million tokens</strong> ($/1M tokens).
          Example: An input rate of 2.50 means $2.50 per 1,000,000 input tokens.
        </p>
        <p>
          <strong>Token Types:</strong>
        </p>
        <ul class="list-disc list-inside ml-4 space-y-1">
          <li><strong>Input:</strong> Standard prompt tokens</li>
          <li><strong>Output:</strong> Completion/response tokens</li>
          <li><strong>Cached:</strong> Cached input tokens (typically 50% of input rate)</li>
          <li><strong>Reasoning:</strong> Special reasoning/thinking tokens (fallback to output rate)</li>
          <li><strong>Cache Creation:</strong> Tokens used to create cache entries (fallback to input rate)</li>
        </ul>
        <p>
          <strong>Custom Pricing:</strong> You can override default pricing for specific models.
          Reset to defaults anytime to restore standard rates.
        </p>
      </div>
    </Card>

    <!-- Current Pricing Preview -->
    <Card class-name="p-6">
      <div class="flex items-center justify-between mb-4">
        <h2 class="text-xl font-semibold">Current Pricing Overview</h2>
        <button
          type="button"
          class="text-primary hover:underline text-sm"
          @click="showModal = true"
        >
          View Full Details
        </button>
      </div>

      <div v-if="loading" class="text-center py-4 text-text-muted">Loading pricing data...</div>
      <div v-else-if="currentPricing" class="space-y-3">
        <div v-for="provider in Object.keys(currentPricing).slice(0, 5)" :key="provider" class="text-sm">
          <span class="font-semibold">{{ provider.toUpperCase() }}:</span>{{ " " }}
          <span class="text-text-muted">
            {{ Object.keys(currentPricing[provider]).length }} models
          </span>
        </div>
        <div v-if="Object.keys(currentPricing).length > 5" class="text-sm text-text-muted">
          + {{ Object.keys(currentPricing).length - 5 }} more providers
        </div>
      </div>
      <div v-else class="text-text-muted">No pricing data available</div>
    </Card>

    <!-- Pricing Modal -->
    <PricingModal
      v-if="showModal"
      :is-open="showModal"
      @close="showModal = false"
      @save="handlePricingUpdated"
    />
  </div>
</template>
