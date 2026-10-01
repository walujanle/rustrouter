<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";

import ProviderIcon from "@/components/ui/ProviderIcon.vue";

const CLI_TOOLS = [
	{ id: "claude", name: "Claude Code", image: "/providers/claude.png" },
	{ id: "codex", name: "OpenAI Codex", image: "/providers/codex.png" },
	{ id: "hermes", name: "Hermes", image: "/providers/hermes.png" },
];

const PROVIDERS = [
	{
		id: "openrouter",
		name: "OpenRouter",
		color: "bg-emerald-500",
		textColor: "text-white",
	},
	{
		id: "deepseek",
		name: "DeepSeek",
		color: "bg-orange-400",
		textColor: "text-white",
	},
	{
		id: "mistral",
		name: "Mistral",
		color: "bg-blue-500",
		textColor: "text-white",
	},
	{
		id: "nvidia",
		name: "NVIDIA",
		color: "bg-gray-700",
		textColor: "text-white",
	},
];

const activeFlow = ref(0);
let interval: ReturnType<typeof setInterval> | null = null;

onMounted(() => {
	interval = setInterval(() => {
		activeFlow.value = (activeFlow.value + 1) % PROVIDERS.length;
	}, 2000);
});

onBeforeUnmount(() => {
	if (interval) clearInterval(interval);
});
</script>

<template>
  <div class="mt-16 w-full max-w-4xl relative h-90 hidden md:flex items-center justify-center animate-[float_6s_ease-in-out_infinite]">
    <!-- RustRouter Hub - Center -->
    <div class="relative z-20 w-32 h-32 rounded-full bg-[#23180f] border-2 border-[#f97815] shadow-[0_0_40px_rgba(249,120,21,0.3)] flex flex-col items-center justify-center gap-1 group cursor-pointer hover:scale-105 transition-transform duration-500">
      <span class="material-symbols-outlined text-4xl text-[#f97815]">hub</span>
      <span class="text-xs font-bold text-white tracking-widest uppercase">RustRouter</span>
      <div class="absolute inset-0 rounded-full border border-[#f97815]/30 animate-ping opacity-20"></div>
    </div>

    <!-- CLI Tools - Left side -->
    <div class="absolute left-0 top-1/2 -translate-y-1/2 flex flex-col gap-7">
      <div
        v-for="tool in CLI_TOOLS"
        :key="tool.id"
        class="flex items-center gap-3 opacity-70 hover:opacity-100 transition-opacity group"
      >
        <div class="w-16 h-16 rounded-2xl bg-[#23180f] border border-[#3a2f27] flex items-center justify-center overflow-hidden p-2 hover:border-[#f97815]/50 transition-all hover:scale-105">
          <ProviderIcon
            :src="tool.image"
            :alt="tool.name"
            :size="48"
            class="object-contain rounded-xl max-w-12 max-h-12"
            :fallback-text="tool.name.slice(0, 2).toUpperCase()"
          />
        </div>
      </div>
    </div>

    <!-- SVG Lines from CLI to RustRouter -->
    <svg
      class="absolute inset-0 w-full h-full z-10 pointer-events-none stroke-yellow-700"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <path
        class="animate-[dash_2s_linear_infinite]"
        d="M 60 50 C 250 70, 250 180, 360 180"
        fill="none"
        stroke-dasharray="5,5"
        stroke-width="2"
      ></path>
      <path
        class="animate-[dash_2s_linear_infinite]"
        d="M 60 140 C 250 140, 250 180, 360 180"
        fill="none"
        stroke-dasharray="5,5"
        stroke-width="2"
      ></path>
      <path
        class="animate-[dash_2s_linear_infinite]"
        d="M 60 210 C 250 210, 250 180, 360 180"
        fill="none"
        stroke-dasharray="5,5"
        stroke-width="2"
      ></path>
      <path
        class="animate-[dash_2s_linear_infinite]"
        d="M 60 300 C 250 280, 250 180, 360 180"
        fill="none"
        stroke-dasharray="5,5"
        stroke-width="2"
      ></path>
    </svg>

    <!-- SVG Lines from RustRouter to Providers -->
    <svg
      class="absolute inset-0 w-full h-full z-10 pointer-events-none"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <path
        d="M 440 180 C 550 180, 550 50, 740 50"
        fill="none"
        :stroke="activeFlow === 0 ? '#f97815' : 'rgb(75, 85, 99)'"
        :stroke-width="activeFlow === 0 ? '3' : '2'"
        :class="activeFlow === 0 ? 'animate-pulse' : ''"
      ></path>
      <path
        d="M 440 180 C 550 180, 550 130, 740 130"
        fill="none"
        :stroke="activeFlow === 1 ? '#f97815' : 'rgb(75, 85, 99)'"
        :stroke-width="activeFlow === 1 ? '3' : '2'"
        :class="activeFlow === 1 ? 'animate-pulse' : ''"
      ></path>
      <path
        d="M 440 180 C 550 180, 550 230, 740 230"
        fill="none"
        :stroke="activeFlow === 2 ? '#f97815' : 'rgb(75, 85, 99)'"
        :stroke-width="activeFlow === 2 ? '3' : '2'"
        :class="activeFlow === 2 ? 'animate-pulse' : ''"
      ></path>
      <path
        d="M 440 180 C 550 180, 550 310, 740 310"
        fill="none"
        :stroke="activeFlow === 3 ? '#f97815' : 'rgb(75, 85, 99)'"
        :stroke-width="activeFlow === 3 ? '3' : '2'"
        :class="activeFlow === 3 ? 'animate-pulse' : ''"
      ></path>
    </svg>

    <!-- AI Providers - Right side -->
    <div class="absolute right-0 top-0 bottom-0 flex flex-col justify-between py-6">
      <div
        v-for="(provider, idx) in PROVIDERS"
        :key="provider.id"
        class="px-4 py-2 rounded-lg flex items-center justify-center font-bold text-xs shadow-lg hover:scale-110 transition-all cursor-help min-w-35"
        :class="[provider.color, provider.textColor, activeFlow === idx ? 'ring-4 ring-[#f97815]/50 scale-110' : '']"
        :title="provider.name"
      >
        {{ provider.name }}
      </div>
    </div>
  </div>

  <!-- Mobile fallback: a sibling of the diagram, not a child — the diagram root is
       `hidden md:flex`, so nesting this inside it made it dead at every width. -->
  <div class="md:hidden mt-8 w-full max-w-4xl p-4 rounded-lg bg-[#23180f] border border-[#3a2f27]">
    <p class="text-sm text-center text-gray-400">Interactive diagram visible on desktop</p>
  </div>
</template>
