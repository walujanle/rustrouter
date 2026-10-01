<script setup lang="ts">
import { computed } from "vue";
import { RouterLink } from "vue-router";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import Badge from "@/components/ui/UiBadge.vue";
import Card from "@/components/ui/UiCard.vue";
import Toggle from "@/components/ui/UiToggle.vue";
import { useProviders } from "@/constants/providers";

// A connection in cooldown (a future `modelLock_*` stamp) is not an error, so it
// still counts as active.

const props = withDefaults(
	defineProps<{
		provider: Record<string, any>;
		kind: string;
		connections: Array<Record<string, any>>;
		isCustom?: boolean;
	}>(),
	{ isCustom: false },
);

const emit = defineEmits<{ toggle: [providerId: string, newActive: boolean] }>();

const { AI_PROVIDERS } = useProviders();

function getEffectiveStatus(conn: Record<string, any>): string {
	const isCooldown = Object.entries(conn).some(
		([k, v]) => k.startsWith("modelLock_") && v && new Date(v as string).getTime() > Date.now(),
	);
	return conn.testStatus === "unavailable" && !isCooldown ? "active" : conn.testStatus;
}

const providerInfo = computed(() => AI_PROVIDERS[props.provider.id]);
const isNoAuth = computed(() => !!providerInfo.value?.noAuth);

const providerConns = computed(() => props.connections.filter((c) => c.provider === props.provider.id));
const connected = computed(
	() =>
		providerConns.value.filter((c) => {
			const s = getEffectiveStatus(c);
			return s === "active" || s === "success";
		}).length,
);
const errorCount = computed(
	() =>
		providerConns.value.filter((c) => {
			const s = getEffectiveStatus(c);
			return s === "error" || s === "expired" || s === "unavailable";
		}).length,
);
const total = computed(() => providerConns.value.length);
const allDisabled = computed(() => total.value > 0 && providerConns.value.every((c) => c.isActive === false));

const iconBg = computed(() => {
	const color = props.provider.color;
	return color && color.length > 7 ? color : `${color ?? "#888"}15`;
});

function handleToggle(active: boolean) {
	emit("toggle", props.provider.id, active);
}
</script>

<template>
  <RouterLink :to="`/dashboard/media-providers/${kind}/${provider.id}`" class="group block">
    <Card
      padding="xs"
      :class="['h-full hover:bg-black/1 dark:hover:bg-white/1 transition-colors cursor-pointer', allDisabled && 'opacity-50']"
    >
      <div class="flex min-w-0 items-center justify-between gap-3">
        <div class="flex min-w-0 items-center gap-3">
          <div class="size-8 rounded-lg flex items-center justify-center shrink-0" :style="{ backgroundColor: iconBg }">
            <ProviderIcon
              :src="`/providers/${provider.id}.png`"
              :alt="provider.name"
              :size="30"
              class="object-contain rounded-lg max-w-7.5 max-h-7.5"
              :fallback-text="provider.textIcon || provider.id.slice(0, 2).toUpperCase()"
              :fallback-color="provider.color"
            />
          </div>
          <div class="min-w-0">
            <h3 class="font-semibold text-sm">{{ provider.name }}</h3>
            <div class="flex items-center gap-2 mt-0.5 flex-wrap">
              <Badge v-if="isCustom" variant="default" size="sm">Custom</Badge>
              <Badge v-if="isNoAuth" variant="success" size="sm">Ready</Badge>
              <Badge v-else-if="allDisabled" variant="default" size="sm">Disabled</Badge>
              <span v-else-if="total === 0" class="text-xs text-text-muted">No connections</span>
              <template v-else>
                <Badge v-if="connected > 0" variant="success" size="sm" dot>{{ connected }} Connected</Badge>
                <Badge v-if="errorCount > 0" variant="error" size="sm" dot>{{ errorCount }} Error</Badge>
                <Badge v-if="connected === 0 && errorCount === 0" variant="default" size="sm">{{ total }} Added</Badge>
              </template>
            </div>
          </div>
        </div>
        <div
          v-if="total > 0"
          class="shrink-0 opacity-100 transition-opacity sm:opacity-0 sm:group-hover:opacity-100"
        >
          <Toggle
            :model-value="!allDisabled"
            size="sm"
            :aria-label="allDisabled ? 'Enable provider' : 'Disable provider'"
            :title="allDisabled ? 'Enable provider' : 'Disable provider'"
            @click.stop.prevent
            @update:model-value="handleToggle"
          />
        </div>
      </div>
    </Card>
  </RouterLink>
</template>
