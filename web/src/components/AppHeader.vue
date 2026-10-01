<script setup lang="ts">
import { computed } from "vue";
import { RouterLink, useRoute } from "vue-router";

import HeaderMenu from "@/components/HeaderMenu.vue";
import ProviderIcon from "@/components/ui/ProviderIcon.vue";
import ThemeToggle from "@/components/ui/ThemeToggle.vue";
import { useProviders } from "@/constants/providers";
import { useHeaderSearchStore } from "@/stores/headerSearch";
import { getProviderIconSrc } from "@/utils/providerIcon";

const props = withDefaults(defineProps<{ onMenuClick?: () => void; showMenuButton?: boolean }>(), {
	showMenuButton: true,
});

const route = useRoute();
const headerSearch = useHeaderSearchStore();
const { OAUTH_PROVIDERS, APIKEY_PROVIDERS, AI_PROVIDERS, MEDIA_PROVIDER_KINDS } = useProviders();

interface Breadcrumb {
	label: string;
	href?: string;
	image?: string | null;
}

const pageInfo = computed<{
	title: string;
	description: string;
	icon?: string;
	breadcrumbs: Breadcrumb[];
}>(() => {
	const pathname = route.path;

	// Media provider detail: /dashboard/media-providers/[kind]/[id]. Checked
	// before the generic "/providers" branch, which would otherwise match.
	const mediaDetailMatch = pathname.match(/\/media-providers\/([^/]+)\/([^/]+)$/);
	if (mediaDetailMatch) {
		const kindId = mediaDetailMatch[1];
		const providerId = mediaDetailMatch[2];
		const kindConfig = MEDIA_PROVIDER_KINDS.find((k) => k.id === kindId);
		const provider = AI_PROVIDERS[providerId];
		return {
			title: provider?.name || providerId,
			description: "",
			breadcrumbs: [
				{ label: "Media Providers", href: `/dashboard/media-providers/${kindId}` },
				{ label: kindConfig?.label || kindId, href: `/dashboard/media-providers/${kindId}` },
				{ label: provider?.name || providerId, image: getProviderIconSrc(providerId) },
			],
		};
	}

	// Media combo editor: /dashboard/media-providers/combo/[id].
	const mediaComboMatch = pathname.match(/\/media-providers\/combo\/[^/]+$/);
	if (mediaComboMatch) {
		return {
			title: "Media Combo",
			description: "Fallback order for a media provider combo",
			icon: "layers",
			breadcrumbs: [],
		};
	}

	// Media provider kind: /dashboard/media-providers/[kind]. The combined /web
	// page has no kind config, so it falls back to a generic title.
	const mediaKindMatch = pathname.match(/\/media-providers\/([^/]+)$/);
	if (mediaKindMatch) {
		const kindId = mediaKindMatch[1];
		if (kindId === "web")
			return {
				title: "Web Fetch & Search",
				description: "Search and fetch providers",
				icon: "travel_explore",
				breadcrumbs: [],
			};
		const kindConfig = MEDIA_PROVIDER_KINDS.find((k) => k.id === kindId);
		return {
			title: kindConfig?.label || kindId,
			description: `Manage your ${kindConfig?.label || kindId} providers`,
			icon: kindConfig?.icon || "perm_media",
			breadcrumbs: [],
		};
	}

	const providerMatch = pathname.match(/\/providers\/([^/]+)$/);
	if (providerMatch) {
		const providerId = providerMatch[1];
		const providerInfo = OAUTH_PROVIDERS[providerId] || APIKEY_PROVIDERS[providerId];
		if (providerInfo) {
			const breadcrumbs: Breadcrumb[] = [
				{ label: "Providers", href: "/dashboard/providers" },
				{ label: providerInfo.name, image: getProviderIconSrc(providerInfo.id) },
			];
			return { title: providerInfo.name, description: "", breadcrumbs };
		}
	}

	if (pathname.includes("/providers"))
		return { title: "Providers", description: "Manage your AI provider connections", icon: "dns", breadcrumbs: [] };
	if (pathname.includes("/combos"))
		return { title: "Combos", description: "Model combos with fallback", icon: "layers", breadcrumbs: [] };
	if (pathname.includes("/usage"))
		return {
			title: "Usage & Analytics",
			description: "Monitor your API usage, token consumption, and request logs",
			icon: "bar_chart",
			breadcrumbs: [],
		};
	if (pathname.includes("/quota"))
		return { title: "Quota Tracker", description: "Track and manage your API quota limits", icon: "data_usage", breadcrumbs: [] };
	if (pathname.includes("/token-saver"))
		return { title: "Token Saver", description: "Compress prompts and outputs to save tokens", icon: "savings", breadcrumbs: [] };
	if (pathname.includes("/cli-tools"))
		return { title: "CLI Tools", description: "Configure CLI tools", icon: "terminal", breadcrumbs: [] };
	if (pathname.includes("/proxy-pools"))
		return { title: "Proxy Pools", description: "Manage your proxy pool configurations", icon: "lan", breadcrumbs: [] };
	if (pathname.includes("/skills"))
		return {
			title: "Agent Skills",
			description: "Copy a link and paste to your AI to use 9Router — no install needed",
			icon: "extension",
			breadcrumbs: [],
		};
	if (pathname.includes("/endpoint"))
		return { title: "Endpoint", description: "API endpoint configuration", icon: "api", breadcrumbs: [] };
	if (pathname.includes("/profile"))
		return { title: "Settings", description: "Manage your preferences", icon: "settings", breadcrumbs: [] };
	if (pathname.includes("/translator"))
		return { title: "Translator", description: "Debug translation flow between formats", icon: "translate", breadcrumbs: [] };
	if (pathname.includes("/console-log"))
		return { title: "Console Log", description: "Live server console output", icon: "monitor", breadcrumbs: [] };
	if (pathname === "/dashboard")
		return { title: "Endpoint", description: "API endpoint configuration", icon: "api", breadcrumbs: [] };
	return { title: "", description: "", breadcrumbs: [] };
});

async function handleLogout() {
	try {
		const res = await fetch("/api/auth/logout", { method: "POST" });
		if (res.ok) window.location.assign("/login");
	} catch (err) {
		console.error("Failed to logout:", err);
	}
}
</script>

<template>
  <header class="shrink-0 flex items-center justify-between gap-3 px-4 lg:px-8 pt-3 pb-2 border-b border-border-subtle bg-surface/60 backdrop-blur-xl lg:bg-transparent lg:backdrop-blur-none z-20">
    <div class="flex items-center gap-3 lg:hidden shrink-0">
      <button type="button" v-if="props.showMenuButton" aria-label="Open navigation menu" title="Open navigation menu" class="text-text-main hover:text-primary transition-colors" @click="props.onMenuClick?.()">
        <span class="material-symbols-outlined">menu</span>
      </button>
    </div>

    <div class="flex flex-col min-w-0 flex-1">
      <div v-if="pageInfo.breadcrumbs.length > 0" class="flex items-center gap-2">
        <div v-for="(crumb, index) in pageInfo.breadcrumbs" :key="`${crumb.label}-${crumb.href || 'current'}`" class="flex items-center gap-2">
          <span v-if="index > 0" class="material-symbols-outlined text-text-muted text-base">chevron_right</span>
          <RouterLink v-if="crumb.href" :to="crumb.href" class="text-text-muted hover:text-primary transition-colors">
            {{ crumb.label }}
          </RouterLink>
          <div v-else class="flex items-center gap-2">
            <ProviderIcon
              v-if="crumb.image"
              :src="crumb.image"
              :alt="crumb.label"
              :size="28"
              class="object-contain rounded max-w-7 max-h-7"
              :fallback-text="crumb.label.slice(0, 2).toUpperCase()"
            />
            <h1 class="text-base lg:text-2xl font-semibold text-text-main tracking-tight truncate">{{ crumb.label }}</h1>
          </div>
        </div>
      </div>
      <div v-else-if="pageInfo.title">
        <div class="flex items-center gap-2">
          <span v-if="pageInfo.icon" class="material-symbols-outlined text-primary text-xl lg:text-2xl">{{ pageInfo.icon }}</span>
          <h1 class="text-base lg:text-2xl font-semibold tracking-tight truncate">{{ pageInfo.title }}</h1>
        </div>
        <p v-if="pageInfo.description" class="hidden lg:block text-sm text-text-muted truncate">{{ pageInfo.description }}</p>
      </div>
    </div>

    <div class="flex items-center gap-1 shrink-0">
      <div v-if="headerSearch.visible" class="relative w-40 sm:w-55">
        <span class="material-symbols-outlined absolute left-2 top-1/2 -translate-y-1/2 text-text-muted text-[16px] pointer-events-none">search</span>
        <input
          type="text"
          :value="headerSearch.query"
          :placeholder="headerSearch.placeholder"
          class="w-full h-8 pl-7 pr-7 rounded-lg border border-border bg-surface/60 text-sm focus:outline-none focus:border-primary/50 transition-colors"
          @input="headerSearch.setQuery(($event.target as HTMLInputElement).value)"
        />
        <button
          v-if="headerSearch.query"
          type="button"
          class="absolute right-1 top-1/2 -translate-y-1/2 text-text-muted hover:text-text-main p-0.5 rounded"
          aria-label="Clear search"
          @click="headerSearch.setQuery('')"
        >
          <span class="material-symbols-outlined text-[16px]">close</span>
        </button>
      </div>
      <ThemeToggle />
      <HeaderMenu @logout="handleLogout" />
    </div>
  </header>
</template>
