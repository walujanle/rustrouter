<script setup lang="ts">
import { RouterLink } from "vue-router";

import { APP_CONFIG } from "@/constants/config";
</script>

<template>
  <section class="relative pt-32 pb-20 px-6 min-h-[90vh] flex flex-col items-center justify-center overflow-hidden">
    <div class="absolute top-0 left-1/2 -translate-x-1/2 w-250 h-125 bg-[#f97815]/10 rounded-full blur-[120px] pointer-events-none"></div>

    <div class="relative z-10 max-w-4xl w-full text-center flex flex-col items-center gap-8">
      <div class="inline-flex items-center gap-2 rounded-full border border-[#3a2f27] bg-[#23180f]/50 px-3 py-1 text-xs font-medium text-[#f97815]">
        <span class="flex h-2 w-2 rounded-full bg-[#f97815] animate-pulse"></span>
        v{{ APP_CONFIG.version }}
      </div>

      <h1 class="text-5xl md:text-7xl font-black leading-[1.1] tracking-tight">
        One Endpoint for <br />
        <span class="text-[#f97815]">All AI Providers</span>
      </h1>

      <p class="text-lg md:text-xl text-gray-400 max-w-2xl mx-auto font-light">
        A local AI routing gateway in Rust, with a web dashboard. Point Claude Code, OpenAI Codex, Cursor, or any OpenAI-compatible client at one endpoint and let it route to the provider you configured.
      </p>

      <div class="flex flex-wrap items-center justify-center gap-4 w-full">
        <RouterLink
          to="/dashboard"
          class="h-12 px-8 rounded-lg bg-[#f97815] hover:bg-[#e0650a] text-[#181411] text-base font-bold transition-all shadow-[0_0_15px_rgba(249,120,21,0.4)] flex items-center gap-2"
        >
          <span class="material-symbols-outlined">rocket_launch</span>
          Get Started
        </RouterLink>
        <a
          href="https://github.com/walujanle/rustrouter"
          target="_blank"
          rel="noopener noreferrer"
          class="h-12 px-8 rounded-lg border border-[#3a2f27] bg-[#23180f] hover:bg-[#3a2f27] text-white text-base font-bold transition-all flex items-center gap-2"
        >
          <span class="material-symbols-outlined">code</span>
          View on GitHub
        </a>
      </div>
    </div>
  </section>
</template>
