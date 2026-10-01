import { createRouter, createWebHistory } from "vue-router";

import { get } from "@/utils/api";

// Dashboard routes render inside `DashboardLayout`; the auth routes render bare.
const routes = [
	{ path: "/", redirect: "/dashboard" },
	{
		path: "/landing",
		name: "landing",
		component: () => import("@/views/LandingPage.vue"),
	},
	{
		path: "/login",
		name: "login",
		component: () => import("@/views/LoginPage.vue"),
	},
	{
		path: "/callback",
		name: "callback",
		component: () => import("@/views/CallbackPage.vue"),
	},
	{
		path: "/dashboard",
		component: () => import("@/components/layouts/DashboardLayout.vue"),
		children: [
			{
				path: "",
				name: "dashboard",
				component: () => import("@/views/EndpointPage.vue"),
			},
			{
				path: "endpoint",
				name: "endpoint",
				component: () => import("@/views/EndpointConfigPage.vue"),
			},
			{
				path: "providers",
				name: "providers",
				component: () => import("@/views/ProvidersPage.vue"),
			},
			{
				path: "providers/new",
				name: "provider-new",
				component: () => import("@/views/ProviderNewPage.vue"),
			},
			{
				path: "providers/:id",
				name: "provider-detail",
				component: () => import("@/views/ProviderDetailPage.vue"),
			},
			{
				path: "combos",
				name: "combos",
				component: () => import("@/views/CombosPage.vue"),
			},
			{
				path: "usage",
				name: "usage",
				component: () => import("@/views/UsagePage.vue"),
			},
			{
				path: "quota",
				name: "quota",
				component: () => import("@/views/QuotaPage.vue"),
			},
			{
				path: "token-saver",
				name: "token-saver",
				component: () => import("@/views/TokenSaverPage.vue"),
			},
			{
				path: "cli-tools",
				name: "cli-tools",
				component: () => import("@/views/CliToolsPage.vue"),
			},
			{
				path: "cli-tools/:toolId",
				name: "cli-tool-detail",
				component: () => import("@/views/CliToolDetailPage.vue"),
			},
			{
				path: "console-log",
				name: "console-log",
				component: () => import("@/views/ConsoleLogPage.vue"),
			},
			{
				path: "translator",
				name: "translator",
				component: () => import("@/views/TranslatorPage.vue"),
			},
			{
				path: "proxy-pools",
				name: "proxy-pools",
				component: () => import("@/views/ProxyPoolsPage.vue"),
			},
			{
				path: "skills",
				name: "skills",
				component: () => import("@/views/SkillsPage.vue"),
			},
			{
				path: "media-providers/web",
				name: "media-providers-web",
				component: () =>
					import("@/views/media-providers/MediaProvidersWebPage.vue"),
			},
			{
				path: "media-providers/combo/:id",
				name: "media-combo-detail",
				component: () =>
					import("@/views/media-providers/MediaComboDetailPage.vue"),
				props: true,
			},
			{
				path: "media-providers/:kind",
				name: "media-provider-kind",
				component: () =>
					import("@/views/media-providers/MediaProviderKindPage.vue"),
				props: true,
			},
			{
				path: "media-providers/:kind/:id",
				name: "media-provider-detail",
				component: () =>
					import("@/views/media-providers/MediaProviderDetailPage.vue"),
				props: true,
			},
			{
				path: "profile",
				name: "profile",
				component: () => import("@/views/ProfilePage.vue"),
			},
			{
				path: "basic-chat",
				name: "basic-chat",
				component: () => import("@/views/BasicChatPage.vue"),
			},
			{
				path: "settings/pricing",
				name: "pricing",
				component: () => import("@/views/PricingPage.vue"),
			},
		],
	},
	{
		path: "/:pathMatch(.*)*",
		name: "not-found",
		component: () => import("@/views/NotFoundPage.vue"),
	},
];

export const router = createRouter({
	history: createWebHistory(),
	routes,
	scrollBehavior() {
		return { top: 0 };
	},
});

// Per-route document title. The landing page keeps the full marketing title;
// every other route is "<Name> · RustRouter", derived from the route name so a
// new route needs no second edit.
const LANDING_TITLE = "RustRouter - AI Infrastructure Management";
router.afterEach((to) => {
	if (to.name === "landing") {
		document.title = LANDING_TITLE;
		return;
	}
	const name = typeof to.name === "string" ? to.name : "";
	const label = name
		.split("-")
		.map((part) => part.charAt(0).toUpperCase() + part.slice(1))
		.join(" ");
	document.title = label ? `${label} · RustRouter` : "RustRouter";
});

/**
 * The auth boundary is owned by the Rust backend: it answers 401 on protected
 * `/api/*` and redirects. This guard only mirrors that decision so the SPA does
 * not flash a dashboard it cannot populate. It is not a security control.
 */
router.beforeEach(async (to) => {
	if (!to.path.startsWith("/dashboard")) return true;
	try {
		// `requireLogin` is a settings value that changes only when the user
		// edits it, so it is cached across navigations; `auth/status` is not —
		// a logout must be seen on the next navigation, not after a TTL.
		const { requireLogin } = (await get("/api/settings/require-login", {
			cacheMs: 30_000,
		})) as {
			requireLogin: boolean;
		};
		if (!requireLogin) return true;
		const status = (await get("/api/auth/status")) as {
			authenticated?: boolean;
		};
		if (status?.authenticated) return true;
		return { name: "login", query: { redirect: to.fullPath } };
	} catch {
		// A 401 from either probe means the session is gone.
		return { name: "login", query: { redirect: to.fullPath } };
	}
});
