<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
import { useProviders } from "@/constants/providers";
import ComboList from "./components/ComboList.vue";

// The merged Web Search and Web Fetch listing, each with its own provider grid
// and combo list.

const router = useRouter();
const { AI_PROVIDERS, getProvidersByKind } = useProviders();

const connections = ref<Array<Record<string, any>>>([]);
const combos = ref<Array<Record<string, any>>>([]);

const searchProviders = computed(() => getProvidersByKind("webSearch"));
const fetchProviders = computed(() => getProvidersByKind("webFetch"));
const searchCombos = computed(() => combos.value.filter((c) => c.kind === "webSearch"));
const fetchCombos = computed(() => combos.value.filter((c) => c.kind === "webFetch"));

onMounted(fetchAll);

async function fetchAll() {
	try {
		const [connsRes, combosRes] = await Promise.all([
			fetch("/api/providers", { cache: "no-store" }),
			fetch("/api/combos", { cache: "no-store" }),
		]);
		if (connsRes.ok) connections.value = (await connsRes.json()).connections || [];
		if (combosRes.ok) combos.value = (await combosRes.json()).combos || [];
	} catch {
		/* offline */
	}
}

function getEffectiveStatus(conn: Record<string, any>): string {
	const isCooldown = Object.entries(conn).some(
		([k, v]) => k.startsWith("modelLock_") && v && new Date(v as string).getTime() > Date.now(),
	);
	return conn.testStatus === "unavailable" && !isCooldown ? "active" : conn.testStatus;
}

function providerStats(providerId: string) {
	const providerConns = connections.value.filter((c) => c.provider === providerId);
	return {
		connected: providerConns.filter((c) => {
			const s = getEffectiveStatus(c);
			return s === "active" || s === "success";
		}).length,
		error: providerConns.filter((c) => {
			const s = getEffectiveStatus(c);
			return s === "error" || s === "expired" || s === "unavailable";
		}).length,
		total: providerConns.length,
		allDisabled: providerConns.length > 0 && providerConns.every((c) => c.isActive === false),
	};
}

function iconBg(provider: Record<string, any>): string {
	const color = provider.color;
	return color && color.length > 7 ? color : `${color ?? "#888"}15`;
}

async function handleCreateCombo(kind: string) {
	const base = kind === "webSearch" ? "search-combo" : "fetch-combo";
	let name = base;
	let i = 1;
	const existing = new Set(combos.value.map((c) => c.name));
	while (existing.has(name)) {
		name = `${base}-${i++}`;
	}
	const res = await fetch("/api/combos", {
		method: "POST",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify({ name, models: [], kind }),
	});
	if (res.ok) {
		const created = await res.json();
		router.push(`/dashboard/media-providers/combo/${created.id}`);
	} else {
		const err = await res.json().catch(() => ({}));
		alert(err.error || "Failed to create combo");
	}
}
</script>

<template>
  <div class="flex flex-col gap-8">
    <!-- Web Search -->
    <section>
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between mb-3">
        <div class="flex flex-wrap items-center gap-2">
          <span class="material-symbols-outlined text-primary">search</span>
          <h2 class="text-base font-semibold">Web Search</h2>
          <span class="text-xs text-text-muted">
            ({{ searchProviders.length }} providers · {{ searchCombos.length }} combos)
          </span>
        </div>
        <Button size="sm" icon="add" @click="handleCreateCombo('webSearch')">Create Combo</Button>
      </div>

      <div v-if="searchCombos.length > 0" class="mb-4">
        <ComboList :combos="searchCombos" />
      </div>

      <div
        v-if="searchProviders.length === 0"
        class="text-center py-8 border border-dashed border-border rounded-xl text-text-muted text-sm"
      >
        No providers.
      </div>
      <div v-else class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
        <RouterLink
          v-for="p in searchProviders"
          :key="p.id"
          :to="`/dashboard/media-providers/webSearch/${p.id}`"
          class="group block"
        >
          <Card
            padding="xs"
            :class="['h-full hover:bg-black/1 dark:hover:bg-white/1 transition-colors cursor-pointer', providerStats(p.id).allDisabled && 'opacity-50']"
          >
            <div class="flex min-w-0 items-center gap-3">
              <div class="size-8 rounded-lg flex items-center justify-center shrink-0" :style="{ backgroundColor: iconBg(p) }">
                <ProviderIcon
                  :src="`/providers/${p.id}.png`"
                  :alt="p.name"
                  :size="30"
                  class="object-contain rounded-lg max-w-7.5 max-h-7.5"
                  :fallback-text="p.textIcon || p.id.slice(0, 2).toUpperCase()"
                  :fallback-color="p.color"
                />
              </div>
              <div>
                <h3 class="font-semibold text-sm">{{ p.name }}</h3>
                <div class="flex items-center gap-2 mt-0.5 flex-wrap">
                  <Badge v-if="AI_PROVIDERS[p.id]?.noAuth" variant="success" size="sm">Ready</Badge>
                  <Badge v-else-if="providerStats(p.id).allDisabled" variant="default" size="sm">Disabled</Badge>
                  <span v-else-if="providerStats(p.id).total === 0" class="text-xs text-text-muted">No connections</span>
                  <template v-else>
                    <Badge v-if="providerStats(p.id).connected > 0" variant="success" size="sm" dot>
                      {{ providerStats(p.id).connected }} Connected
                    </Badge>
                    <Badge v-if="providerStats(p.id).error > 0" variant="error" size="sm" dot>
                      {{ providerStats(p.id).error }} Error
                    </Badge>
                    <Badge
                      v-if="providerStats(p.id).connected === 0 && providerStats(p.id).error === 0"
                      variant="default"
                      size="sm"
                    >
                      {{ providerStats(p.id).total }} Added
                    </Badge>
                  </template>
                </div>
              </div>
            </div>
          </Card>
        </RouterLink>
      </div>
    </section>

    <div class="border-t border-border" />

    <!-- Web Fetch -->
    <section>
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between mb-3">
        <div class="flex flex-wrap items-center gap-2">
          <span class="material-symbols-outlined text-primary">travel_explore</span>
          <h2 class="text-base font-semibold">Web Fetch</h2>
          <span class="text-xs text-text-muted">
            ({{ fetchProviders.length }} providers · {{ fetchCombos.length }} combos)
          </span>
        </div>
        <Button size="sm" icon="add" @click="handleCreateCombo('webFetch')">Create Combo</Button>
      </div>

      <div v-if="fetchCombos.length > 0" class="mb-4">
        <ComboList :combos="fetchCombos" />
      </div>

      <div
        v-if="fetchProviders.length === 0"
        class="text-center py-8 border border-dashed border-border rounded-xl text-text-muted text-sm"
      >
        No providers.
      </div>
      <div v-else class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
        <RouterLink
          v-for="p in fetchProviders"
          :key="p.id"
          :to="`/dashboard/media-providers/webFetch/${p.id}`"
          class="group block"
        >
          <Card
            padding="xs"
            :class="['h-full hover:bg-black/1 dark:hover:bg-white/1 transition-colors cursor-pointer', providerStats(p.id).allDisabled && 'opacity-50']"
          >
            <div class="flex min-w-0 items-center gap-3">
              <div class="size-8 rounded-lg flex items-center justify-center shrink-0" :style="{ backgroundColor: iconBg(p) }">
                <ProviderIcon
                  :src="`/providers/${p.id}.png`"
                  :alt="p.name"
                  :size="30"
                  class="object-contain rounded-lg max-w-7.5 max-h-7.5"
                  :fallback-text="p.textIcon || p.id.slice(0, 2).toUpperCase()"
                  :fallback-color="p.color"
                />
              </div>
              <div>
                <h3 class="font-semibold text-sm">{{ p.name }}</h3>
                <div class="flex items-center gap-2 mt-0.5 flex-wrap">
                  <Badge v-if="AI_PROVIDERS[p.id]?.noAuth" variant="success" size="sm">Ready</Badge>
                  <Badge v-else-if="providerStats(p.id).allDisabled" variant="default" size="sm">Disabled</Badge>
                  <span v-else-if="providerStats(p.id).total === 0" class="text-xs text-text-muted">No connections</span>
                  <template v-else>
                    <Badge v-if="providerStats(p.id).connected > 0" variant="success" size="sm" dot>
                      {{ providerStats(p.id).connected }} Connected
                    </Badge>
                    <Badge v-if="providerStats(p.id).error > 0" variant="error" size="sm" dot>
                      {{ providerStats(p.id).error }} Error
                    </Badge>
                    <Badge
                      v-if="providerStats(p.id).connected === 0 && providerStats(p.id).error === 0"
                      variant="default"
                      size="sm"
                    >
                      {{ providerStats(p.id).total }} Added
                    </Badge>
                  </template>
                </div>
              </div>
            </div>
          </Card>
        </RouterLink>
      </div>
    </section>
  </div>
</template>
