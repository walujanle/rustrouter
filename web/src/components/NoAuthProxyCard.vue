<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Card from "@/components/ui/UiCard.vue";
import Select from "@/components/ui/UiSelect.vue";

const uid = useId();

const NONE_PROXY_POOL_VALUE = "__none__";
const STRATEGIES = [
	{ value: "none", label: "None (single pool)" },
	{ value: "round-robin", label: "Round-robin" },
	{ value: "random", label: "Random" },
];

const props = defineProps<{ providerId: string }>();

const proxyPools = ref<Array<Record<string, any>>>([]);
const proxyPoolId = ref(NONE_PROXY_POOL_VALUE);
const rotateStrategy = ref("none");
const saving = ref(false);
const savedFlash = ref(false);

let cancelled = false;

function loadConfig() {
	cancelled = false;
	Promise.all([
		fetch("/api/proxy-pools?isActive=true", { cache: "no-store" }).then((r) =>
			r.ok ? r.json() : { proxyPools: [] },
		),
		fetch("/api/settings", { cache: "no-store" }).then((r) => (r.ok ? r.json() : {})),
	])
		.then(([poolData, settingsData]) => {
			if (cancelled) return;
			proxyPools.value = poolData.proxyPools || [];
			const override = (settingsData as Record<string, any>).providerStrategies?.[props.providerId] || {};
			proxyPoolId.value = override.proxyPoolId || NONE_PROXY_POOL_VALUE;
			rotateStrategy.value = override.rotateStrategy || "none";
		})
		.catch(() => {});
}

onMounted(loadConfig);
watch(() => props.providerId, loadConfig);

async function save(poolId: string, strategy: string) {
	saving.value = true;
	try {
		const res = await fetch("/api/settings", { cache: "no-store" });
		const data = (res.ok ? await res.json() : {}) as Record<string, any>;
		const current = data.providerStrategies || {};
		const override = { ...(current[props.providerId] || {}) };
		if (poolId === NONE_PROXY_POOL_VALUE) delete override.proxyPoolId;
		else override.proxyPoolId = poolId;
		if (strategy === "none") delete override.rotateStrategy;
		else override.rotateStrategy = strategy;
		const updated = { ...current };
		if (Object.keys(override).length === 0) delete updated[props.providerId];
		else updated[props.providerId] = override;
		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ providerStrategies: updated }),
		});
		savedFlash.value = true;
		setTimeout(() => {
			savedFlash.value = false;
		}, 1500);
	} catch (e) {
		console.log("Save proxy config error:", e);
	} finally {
		saving.value = false;
	}
}

function handlePoolChange(newPoolId: string) {
	proxyPoolId.value = newPoolId;
	save(newPoolId, rotateStrategy.value);
}

function handleStrategyChange(newStrategy: string) {
	rotateStrategy.value = newStrategy;
	save(proxyPoolId.value, newStrategy);
}

const canRotate = computed(() => proxyPools.value.length >= 2);
const isRotation = computed(() => rotateStrategy.value !== "none");

const poolOptions = computed(() => [
	{ value: NONE_PROXY_POOL_VALUE, label: "None (direct)" },
	...proxyPools.value.map((pool) => ({ value: pool.id, label: pool.name })),
]);

const poolHint = computed(() =>
	isRotation.value ? "Pool selector is ignored when rotation is active — all active pools are used." : undefined,
);

const rotationHint = computed(() => {
	if (!canRotate.value) return "Need at least 2 active proxy pools for rotation.";
	if (isRotation.value) {
		return rotateStrategy.value === "round-robin"
			? `Rotating through all ${proxyPools.value.length} active pools in order. State is in-memory (resets on restart).`
			: `Picking a random pool from ${proxyPools.value.length} active pools each request.`;
	}
	return "Uses the selected pool above. Set to Round-robin or Random to rotate across all active pools.";
});
</script>

<template>
  <Card>
    <div class="flex items-center gap-3 mb-4">
      <div class="inline-flex items-center justify-center w-10 h-10 rounded-full bg-green-500/10 text-green-500">
        <span class="material-symbols-outlined text-[20px]">lock_open</span>
      </div>
      <div class="flex-1">
        <p class="text-sm font-medium">No authentication required</p>
        <p class="text-xs text-text-muted">This provider is ready to use. Optionally route requests through a proxy pool to bypass IP-based limits.</p>
      </div>
      <Badge v-if="savedFlash" variant="success" size="sm">Saved</Badge>
    </div>

    <Select
      label="Proxy Pool"
      :model-value="proxyPoolId"
      :disabled="saving || isRotation"
      :options="poolOptions"
      :hint="poolHint"
      @update:model-value="handlePoolChange"
    />

    <div class="flex flex-col gap-2 mt-4">
      <label :for="`rotation-strategy-${uid}`" class="text-sm font-medium text-text-main">Rotation Strategy</label>
      <select
        :id="`rotation-strategy-${uid}`"
        :value="rotateStrategy"
        :disabled="saving"
        class="py-2 px-3 text-sm text-text-main bg-white dark:bg-white/5 border border-black/10 dark:border-white/10 rounded-md focus:ring-1 focus:ring-primary/30 focus:border-primary/50 focus:outline-none transition-all disabled:opacity-50"
        @change="handleStrategyChange(($event.target as HTMLSelectElement).value)"
      >
        <option
          v-for="s in STRATEGIES"
          :key="s.value"
          :value="s.value"
          :disabled="s.value !== 'none' && !canRotate"
        >
          {{ s.label }}
        </option>
      </select>
      <p class="text-xs text-text-muted">{{ rotationHint }}</p>
    </div>
  </Card>
</template>
