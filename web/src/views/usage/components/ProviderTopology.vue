<script setup lang="ts">
import type { Edge, EdgeTypesObject, Node, NodeTypesObject } from "@vue-flow/core";
import { BaseEdge, getBezierPath, Handle, Panel, Position, VueFlow, type VueFlowStore } from "@vue-flow/core";
import "@vue-flow/core/dist/style.css";
import { computed, defineComponent, h, onBeforeUnmount, onMounted, type PropType, ref, useTemplateRef, watch } from "vue";

import { useProviders } from "@/constants/providers";
import { getProviderIconSrc, markProviderIconMissing } from "@/utils/providerIcon";

// Force-stop FE animation if a provider stays active longer than this
const FE_ACTIVE_TIMEOUT_MS = 60000;
const FE_ACTIVE_TICK_MS = 1000;

// Kame + electric particles along active edges
const KAME_PARTICLE_COUNT = 6;
const SPARK_COUNT = 5;

const HANDLE_CLASS = "!bg-transparent !border-0 !w-0 !h-0";

interface ProviderNodeData {
	label: string;
	color: string;
	imageUrl: string | null;
	textIcon: string;
	active: boolean;
}

const HANDLE_POSITIONS = [Position.Top, Position.Bottom, Position.Left, Position.Right] as const;
const HANDLE_IDS = ["top", "bottom", "left", "right"] as const;

// Custom provider node - rectangle with image + name
const ProviderNode = defineComponent({
	name: "ProviderNode",
	props: { data: { type: Object as PropType<ProviderNodeData>, required: true } },
	setup(props) {
		const imgError = ref(false);
		return () => {
			const { label, color, imageUrl, textIcon, active } = props.data;
			const showImg = imageUrl && !imgError.value;
			return h(
				"div",
				{
					class:
						"flex items-center gap-2.5 px-4 py-2.5 rounded-lg border-2 transition-all duration-300 bg-bg",
					style: {
						borderColor: active ? color : "var(--color-border)",
						boxShadow: active ? `0 0 16px ${color}40` : "none",
						minWidth: "150px",
					},
				},
				[
					...HANDLE_POSITIONS.map((position, i) =>
						h(Handle, { key: position, type: "target", position, id: HANDLE_IDS[i], class: HANDLE_CLASS }),
					),
					h(
						"div",
						{
							class: "w-8 h-8 rounded-md flex items-center justify-center shrink-0",
							style: { backgroundColor: `${color}15` },
						},
						[
							showImg
								? h("img", {
										src: imageUrl as string,
										alt: label,
										class: "w-6 h-6 rounded-sm object-contain",
										loading: "lazy",
										decoding: "async",
										onError: () => {
											const m = imageUrl?.match(/^\/providers\/([^/]+)\.png$/i);
											if (m) markProviderIconMissing(m[1]);
											imgError.value = true;
										},
									})
								: h("span", { class: "text-sm font-bold", style: { color } }, textIcon),
						],
					),
					h(
						"span",
						{ class: "text-base font-medium truncate", style: { color: active ? color : "var(--color-text)" } },
						label,
					),
					active
						? h("span", { class: "relative flex h-2 w-2 shrink-0" }, [
								h("span", {
									class: "animate-ping absolute inline-flex h-full w-full rounded-full opacity-75",
									style: { backgroundColor: color },
								}),
								h("span", { class: "relative inline-flex rounded-full h-2 w-2", style: { backgroundColor: color } }),
							])
						: null,
				],
			);
		};
	},
});

// Center RustRouter node — pulse/glow on card only (no expanding rings)
const RouterNode = defineComponent({
	name: "RouterNode",
	props: { data: { type: Object as PropType<{ activeCount: number }>, required: true } },
	setup(props) {
		return () => {
			const powering = (props.data.activeCount || 0) > 0;
			return h(
				"div",
				{
					class: `relative z-1 flex items-center justify-center px-5 py-3 rounded-xl border-2 min-w-32.5 ${
						powering
							? "topology-router-core border-yellow-300 bg-gradient-to-br from-primary/30 via-yellow-400/20 to-cyan-400/25"
							: "border-primary bg-primary/5 shadow-md"
					}`,
				},
				[
					...HANDLE_POSITIONS.map((position, i) =>
						h(Handle, { key: position, type: "source", position, id: HANDLE_IDS[i], class: HANDLE_CLASS }),
					),
					h("img", {
						src: "/favicon.svg",
						alt: "RustRouter",
						class: `w-6 h-6 mr-2 ${powering ? "topology-router-icon" : ""}`,
						loading: "lazy",
						decoding: "async",
					}),
					h(
						"span",
						{ class: `text-sm font-bold ${powering ? "topology-router-label text-yellow-300" : "text-primary"}` },
						"RustRouter",
					),
					props.data.activeCount > 0
						? h(
								"span",
								{ class: "ml-2 px-1.5 py-0.5 rounded-full bg-yellow-400 text-black text-xs font-bold topology-router-badge" },
								props.data.activeCount,
							)
						: null,
				],
			);
		};
	},
});

// Active: electric kame beam (multi-layer stroke + sparks). Idle/last/error: solid BaseEdge.
const TopologyEdge = defineComponent({
	name: "TopologyEdge",
	props: {
		id: { type: String, required: true },
		sourceX: { type: Number, required: true },
		sourceY: { type: Number, required: true },
		targetX: { type: Number, required: true },
		targetY: { type: Number, required: true },
		sourcePosition: { type: String as PropType<Position>, required: true },
		targetPosition: { type: String as PropType<Position>, required: true },
		style: { type: Object as PropType<Record<string, any>>, default: () => ({}) },
		data: { type: Object as PropType<{ active?: boolean }>, default: () => ({}) },
	},
	setup(props) {
		return () => {
			const [edgePath] = getBezierPath({
				sourceX: props.sourceX,
				sourceY: props.sourceY,
				sourcePosition: props.sourcePosition,
				targetX: props.targetX,
				targetY: props.targetY,
				targetPosition: props.targetPosition,
			});
			const isActive = !!props.data?.active;
			const stroke = props.style?.stroke || "var(--color-border)";
			const filterId = `topo-electric-${props.id}`;

			if (!isActive) {
				return h(BaseEdge, { id: props.id, path: edgePath, style: { ...props.style, stroke } });
			}

			const particles = Array.from({ length: KAME_PARTICLE_COUNT }, (_, i) =>
				h(
					"circle",
					{
						key: `${props.id}-p-${i}`,
						r: i % 2 === 0 ? 4 : 2.5,
						fill: i % 3 === 0 ? "#fde047" : i % 3 === 1 ? "#67e8f9" : "#fff",
						opacity: 0.95,
						style: { filter: "drop-shadow(0 0 4px #22d3ee)" },
					},
					[h("animateMotion", { dur: `${0.4 + i * 0.08}s`, repeatCount: "indefinite", path: edgePath, begin: `${i * 0.09}s` })],
				),
			);

			const sparks = Array.from({ length: SPARK_COUNT }, (_, i) =>
				h("circle", { key: `${props.id}-s-${i}`, r: 1.8, fill: "#e0f2fe", opacity: 0 }, [
					h("animate", {
						attributeName: "opacity",
						values: "0;1;0;0;1;0",
						dur: `${0.35 + (i % 3) * 0.1}s`,
						begin: `${i * 0.07}s`,
						repeatCount: "indefinite",
					}),
					h("animateMotion", { dur: `${0.28 + i * 0.05}s`, repeatCount: "indefinite", path: edgePath, begin: `${i * 0.11}s` }),
				]),
			);

			return h("g", { class: "topology-edge-electric" }, [
				h("defs", [
					h("filter", { id: filterId, x: "-40%", y: "-40%", width: "180%", height: "180%" }, [
						h("feTurbulence", { type: "fractalNoise", baseFrequency: "0.9", numOctaves: "2", seed: "2", result: "noise" }, [
							h("animate", {
								attributeName: "baseFrequency",
								values: "0.8;1.4;0.8",
								dur: "0.25s",
								repeatCount: "indefinite",
							}),
						]),
						h("feDisplacementMap", {
							in: "SourceGraphic",
							in2: "noise",
							scale: "3.5",
							xChannelSelector: "R",
							yChannelSelector: "G",
						}),
					]),
				]),
				h("path", {
					d: edgePath,
					fill: "none",
					stroke: "#22d3ee",
					strokeWidth: 10,
					strokeOpacity: 0.35,
					strokeLinecap: "round",
					filter: `url(#${filterId})`,
					class: "topology-edge-halo",
				}),
				h("path", {
					d: edgePath,
					fill: "none",
					stroke: "#4ade80",
					strokeWidth: 5,
					strokeOpacity: 0.85,
					strokeLinecap: "round",
					filter: `url(#${filterId})`,
					class: "topology-edge-plasma",
				}),
				h(BaseEdge, {
					id: props.id,
					path: edgePath,
					style: { stroke: "#f8fafc", strokeWidth: 2.2, opacity: 1 },
					class: "topology-edge-kame",
				}),
				...particles,
				...sparks,
			]);
		};
	},
});

const props = withDefaults(
	defineProps<{
		providers?: Array<Record<string, any>>;
		activeRequests?: Array<Record<string, any>>;
		lastProvider?: string;
		errorProvider?: string;
	}>(),
	{ providers: () => [], activeRequests: () => [], lastProvider: "", errorProvider: "" },
);

const { AI_PROVIDERS } = useProviders();

const containerRef = useTemplateRef<HTMLDivElement>("container");

function getProviderConfig(providerId: string) {
	return AI_PROVIDERS[providerId] || { color: "#6b7280", name: providerId };
}

// Serialize to stable string keys so the layout only re-runs when values change
const activeKey = computed(() =>
	props.activeRequests
		.map((r) => r.provider?.toLowerCase())
		.filter(Boolean)
		.sort()
		.join(","),
);
const lastKey = computed(() => props.lastProvider?.toLowerCase() || "");
const errorKey = computed(() => props.errorProvider?.toLowerCase() || "");

const rawActiveSet = computed(() => new Set(activeKey.value ? activeKey.value.split(",") : []));
const lastSet = computed(() => new Set(lastKey.value ? [lastKey.value] : []));
const errorSet = computed(() => new Set(errorKey.value ? [errorKey.value] : []));

// Track firstSeen per active provider; drop provider if running too long (BE stuck)
const firstSeen: Record<string, number> = {};
const tick = ref(0);

function syncFirstSeen() {
	const now = Date.now();
	for (const p of rawActiveSet.value) {
		if (!firstSeen[p]) firstSeen[p] = now;
	}
	for (const p of Object.keys(firstSeen)) {
		if (!rawActiveSet.value.has(p)) delete firstSeen[p];
	}
}

const activeSet = computed(() => {
	void tick.value;
	const now = Date.now();
	const filtered = new Set<string>();
	for (const p of rawActiveSet.value) {
		const ts = firstSeen[p];
		if (!ts || now - ts < FE_ACTIVE_TIMEOUT_MS) filtered.add(p);
	}
	return filtered;
});

let tickTimer: ReturnType<typeof setInterval> | null = null;

function stopTick() {
	if (tickTimer) clearInterval(tickTimer);
	tickTimer = null;
}

function startTick() {
	stopTick();
	if (rawActiveSet.value.size === 0) return;
	tickTimer = setInterval(() => {
		syncFirstSeen();
		tick.value += 1;
	}, FE_ACTIVE_TICK_MS);
}

// Place N nodes evenly along an ellipse around the router center.
function buildLayout(
	providers: Array<Record<string, any>>,
	active: Set<string>,
	last: Set<string>,
	error: Set<string>,
): { nodes: Node[]; edges: Edge[] } {
	const nodeW = 180;
	const nodeH = 30;
	const routerW = 120;
	const routerH = 44;
	const nodeGap = 24;

	const count = providers.length;

	const minRx = ((nodeW + nodeGap) * count) / (2 * Math.PI);
	const rx = Math.max(320, minRx);
	const ry = Math.max(200, rx * 0.55);
	if (count === 0) {
		return {
			nodes: [{ id: "router", type: "router", position: { x: 0, y: 0 }, data: { activeCount: 0 }, draggable: false }],
			edges: [],
		};
	}

	const nodes: Node[] = [];
	const edges: Edge[] = [];

	nodes.push({
		id: "router",
		type: "router",
		position: { x: -routerW / 2, y: -routerH / 2 },
		data: { activeCount: active.size },
		draggable: false,
	});

	const edgeStyle = (isActive: boolean, isLast: boolean, isError: boolean) => {
		if (isError) return { stroke: "#ef4444", strokeWidth: 2.5, opacity: 0.9 };
		if (isActive) return { stroke: "#22d3ee", strokeWidth: 3.5, opacity: 1 };
		if (isLast) return { stroke: "#f59e0b", strokeWidth: 2, opacity: 0.7 };
		return { stroke: "var(--color-border)", strokeWidth: 1, opacity: 0.3 };
	};

	providers.forEach((p, i) => {
		const config = getProviderConfig(p.provider);
		const isActive = active.has(p.provider?.toLowerCase());
		const isLast = !isActive && last.has(p.provider?.toLowerCase());
		const isError = !isActive && error.has(p.provider?.toLowerCase());
		const nodeId = `provider-${p.provider}`;
		const data: ProviderNodeData = {
			label: (config.name !== p.provider ? config.name : null) || p.nodeName || p.name || p.provider,
			color: config.color || "#6b7280",
			imageUrl: getProviderIconSrc(p.provider),
			textIcon: config.textIcon || (p.provider || "?").slice(0, 2).toUpperCase(),
			active: isActive,
		};

		// Distribute evenly starting from top (−π/2), clockwise
		const angle = -Math.PI / 2 + (2 * Math.PI * i) / count;
		const cx = rx * Math.cos(angle);
		const cy = ry * Math.sin(angle);

		// Pick router handle closest to the node direction
		let sourceHandle: string;
		let targetHandle: string;
		if (Math.abs(angle + Math.PI / 2) < Math.PI / 4 || Math.abs(angle - (3 * Math.PI) / 2) < Math.PI / 4) {
			sourceHandle = "top";
			targetHandle = "bottom";
		} else if (Math.abs(angle - Math.PI / 2) < Math.PI / 4) {
			sourceHandle = "bottom";
			targetHandle = "top";
		} else if (cx > 0) {
			sourceHandle = "right";
			targetHandle = "left";
		} else {
			sourceHandle = "left";
			targetHandle = "right";
		}

		nodes.push({
			id: nodeId,
			type: "provider",
			position: { x: cx - nodeW / 2, y: cy - nodeH / 2 },
			data,
			draggable: false,
		});

		edges.push({
			id: `e-${nodeId}`,
			type: "topology",
			source: "router",
			sourceHandle,
			target: nodeId,
			targetHandle,
			animated: false,
			data: { active: isActive },
			style: edgeStyle(isActive, isLast, isError),
		});
	});

	return { nodes, edges };
}

const layout = computed(() => buildLayout(props.providers, activeSet.value, lastSet.value, errorSet.value));
const nodes = computed(() => layout.value.nodes);
const edges = computed(() => layout.value.edges);

const nodeTypes = { provider: ProviderNode, router: RouterNode } as unknown as NodeTypesObject;
const edgeTypes = { topology: TopologyEdge } as unknown as EdgeTypesObject;

const fitOpts = { padding: 0.2, duration: 200 };
const flow = ref<VueFlowStore | null>(null);
let resizeObserver: ResizeObserver | null = null;

function fit() {
	flow.value?.fitView(fitOpts);
}

function onFlowInit(instance: VueFlowStore) {
	flow.value = instance;
	setTimeout(fit, 50);
}

function refit() {
	setTimeout(fit, 50);
}

onMounted(() => {
	syncFirstSeen();
	startTick();
	const el = containerRef.value;
	if (el) {
		resizeObserver = new ResizeObserver(fit);
		resizeObserver.observe(el);
	}
});

onBeforeUnmount(() => {
	stopTick();
	resizeObserver?.disconnect();
});

watch(() => nodes.value.length, refit);
watch(rawActiveSet, () => {
	syncFirstSeen();
	startTick();
});
</script>

<template>
  <div ref="container" class="h-80 w-full min-w-0 rounded-lg border border-border bg-surface-2/30 sm:h-120">
    <div v-if="props.providers.length === 0" class="h-full flex items-center justify-center text-text-muted text-sm">
      No providers connected
    </div>
    <VueFlow
      v-else
      :nodes="nodes"
      :edges="edges"
      :node-types="nodeTypes"
      :edge-types="edgeTypes"
      fit-view-on-init
      :min-zoom="0.1"
      :max-zoom="2"
      pan-on-drag
      zoom-on-scroll
      zoom-on-pinch
      zoom-on-double-click
      :prevent-scrolling="false"
      :nodes-draggable="false"
      :nodes-connectable="false"
      :elements-selectable="false"
      @init="onFlowInit"
    >
      <Panel position="bottom-left" class="vue-flow-controls-custom flex flex-col">
        <button type="button" aria-label="Zoom in" @click="flow?.zoomIn()">+</button>
        <button type="button" aria-label="Zoom out" @click="flow?.zoomOut()">−</button>
        <button type="button" aria-label="Fit view" @click="fit()">⤢</button>
      </Panel>
    </VueFlow>
  </div>
</template>
