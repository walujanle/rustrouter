<script setup lang="ts">
import { ref, watch } from "vue";

type PricingData = Record<string, Record<string, Record<string, number>>>;

const props = defineProps<{
	isOpen: boolean;
}>();

const emit = defineEmits<{ close: []; save: [] }>();

const pricingData = ref<PricingData>({});
const loading = ref(true);
const saving = ref(false);

watch(
	() => props.isOpen,
	(open) => {
		if (open) loadPricing();
	},
	{ immediate: true },
);

// `GET /api/pricing` already returns the built-in pricing table merged with
// user overrides, so an empty map is the only local fallback left.
const FALLBACK_PRICING: PricingData = {};

async function loadPricing() {
	loading.value = true;
	try {
		const response = await fetch("/api/pricing");
		if (response.ok) {
			const data = await response.json();
			pricingData.value = data;
		} else {
			// Fallback to defaults
			pricingData.value = FALLBACK_PRICING;
		}
	} catch (error) {
		console.error("Failed to load pricing:", error);
		pricingData.value = FALLBACK_PRICING;
	} finally {
		loading.value = false;
	}
}

function handlePricingChange(provider: string, model: string, field: string, value: string) {
	const numValue = Number.parseFloat(value);
	if (Number.isNaN(numValue) || numValue < 0) return;

	const newData = { ...pricingData.value };
	if (!newData[provider]) newData[provider] = {};
	if (!newData[provider][model]) newData[provider][model] = {};
	newData[provider][model][field] = numValue;
	pricingData.value = newData;
}

async function handleSave() {
	saving.value = true;
	try {
		const response = await fetch("/api/pricing", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(pricingData.value),
		});

		if (response.ok) {
			emit("save");
			emit("close");
		} else {
			const error = await response.json();
			alert(`Failed to save pricing: ${error.error}`);
		}
	} catch (error) {
		console.error("Failed to save pricing:", error);
		alert("Failed to save pricing");
	} finally {
		saving.value = false;
	}
}

async function handleReset() {
	if (!confirm("Reset all pricing to defaults? This cannot be undone.")) return;

	try {
		const response = await fetch("/api/pricing", { method: "DELETE" });
		if (response.ok) {
			pricingData.value = FALLBACK_PRICING;
		}
	} catch (error) {
		console.error("Failed to reset pricing:", error);
		alert("Failed to reset pricing");
	}
}

const pricingFields = ["input", "output", "cached", "reasoning", "cache_creation"];
</script>

<template>
  <div v-if="props.isOpen" class="fixed inset-0 bg-black/50 flex items-center justify-center z-50 p-4">
    <div class="bg-surface border border-border rounded-lg shadow-xl max-w-6xl w-full max-h-[90vh] overflow-hidden flex flex-col">
      <div class="p-4 border-b border-border flex items-center justify-between">
        <h2 class="text-xl font-semibold">Pricing Configuration</h2>
        <button
          type="button"
          class="text-text-muted hover:text-text text-2xl leading-none"
          @click="emit('close')"
        >
          ×
        </button>
      </div>

      <div class="flex-1 overflow-auto p-4">
        <div v-if="loading" class="text-center py-8 text-text-muted">Loading pricing data...</div>
        <div v-else class="space-y-6">
          <div class="bg-surface-2 border border-border rounded-lg p-3 text-sm">
            <p class="font-medium mb-1">Pricing Rates Format</p>
            <p class="text-text-muted">
              All rates are in <strong>dollars per million tokens</strong> ($/1M tokens).
              Example: Input rate of 2.50 means $2.50 per 1,000,000 input tokens.
            </p>
          </div>

          <div
            v-for="provider in Object.keys(pricingData).sort()"
            :key="provider"
            class="border border-border rounded-lg overflow-hidden"
          >
            <div class="bg-surface-2 px-4 py-2 font-semibold text-sm">
              {{ provider.toUpperCase() }}
            </div>
            <div class="overflow-x-auto">
              <table class="w-full text-sm">
                <thead class="bg-surface-3 text-text-muted uppercase text-xs">
                  <tr>
                    <th class="px-3 py-2 text-left">Model</th>
                    <th class="px-3 py-2 text-right">Input</th>
                    <th class="px-3 py-2 text-right">Output</th>
                    <th class="px-3 py-2 text-right">Cached</th>
                    <th class="px-3 py-2 text-right">Reasoning</th>
                    <th class="px-3 py-2 text-right">Cache Creation</th>
                  </tr>
                </thead>
                <tbody class="divide-y divide-border">
                  <tr
                    v-for="model in Object.keys(pricingData[provider]).sort()"
                    :key="model"
                    class="hover:bg-surface-2/50"
                  >
                    <td class="px-3 py-2 font-medium">{{ model }}</td>
                    <td v-for="field in pricingFields" :key="field" class="px-3 py-2">
                      <input
                        type="number"
                        step="0.01"
                        min="0"
                        :value="pricingData[provider][model][field] || 0"
                        class="w-20 px-2 py-1 text-right bg-surface border border-border rounded focus:outline-none focus:border-primary"
                        @input="handlePricingChange(provider, model, field, ($event.target as HTMLInputElement).value)"
                      />
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
          </div>

          <div v-if="Object.keys(pricingData).length === 0" class="text-center py-8 text-text-muted">
            No pricing data available
          </div>
        </div>
      </div>

      <div class="p-4 border-t border-border flex items-center justify-between gap-2">
        <button
          type="button"
          class="px-4 py-2 text-sm text-red-500 hover:bg-red-500/10 rounded border border-red-500/20 transition-colors"
          :disabled="saving"
          @click="handleReset"
        >
          Reset to Defaults
        </button>
        <div class="flex gap-2">
          <button
            type="button"
            class="px-4 py-2 text-sm text-text-muted hover:text-text border border-border rounded transition-colors"
            :disabled="saving"
            @click="emit('close')"
          >
            Cancel
          </button>
          <button
            type="button"
            class="px-4 py-2 text-sm bg-primary text-white rounded hover:bg-primary/90 transition-colors disabled:opacity-50"
            :disabled="saving"
            @click="handleSave"
          >
            {{ saving ? "Saving..." : "Save Changes" }}
          </button>
        </div>
      </div>
    </div>
  </div>
</template>
