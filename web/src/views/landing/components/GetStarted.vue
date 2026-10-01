<script setup lang="ts">
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";

const { copied, copy } = useCopyToClipboard();

function handleCopy(text: string) {
	copy(text, "landing");
}
</script>

<template>
  <section class="py-24 px-6 bg-[#120f0d]">
    <div class="max-w-7xl mx-auto">
      <div class="flex flex-col lg:flex-row gap-16 items-start">
        <!-- Left: Steps -->
        <div class="flex-1">
          <h2 class="text-3xl md:text-4xl font-bold mb-6">Install and run</h2>
          <p class="text-gray-400 text-lg mb-8">
            Install RustRouter, add a provider in the dashboard, and point your client at the local endpoint.
          </p>

          <div class="flex flex-col gap-6">
            <div class="flex gap-4">
              <div class="flex-none w-8 h-8 rounded-full bg-[#f97815]/20 text-[#f97815] flex items-center justify-center font-bold">1</div>
              <div>
                <h4 class="font-bold text-lg">Install RustRouter</h4>
                <p class="text-sm text-gray-500 mt-1">One npm command installs the binary</p>
              </div>
            </div>

            <div class="flex gap-4">
              <div class="flex-none w-8 h-8 rounded-full bg-[#f97815]/20 text-[#f97815] flex items-center justify-center font-bold">2</div>
              <div>
                <h4 class="font-bold text-lg">Open Dashboard</h4>
                <p class="text-sm text-gray-500 mt-1">Configure providers and API keys via web interface</p>
              </div>
            </div>

            <div class="flex gap-4">
              <div class="flex-none w-8 h-8 rounded-full bg-[#f97815]/20 text-[#f97815] flex items-center justify-center font-bold">3</div>
              <div>
                <h4 class="font-bold text-lg">Route Requests</h4>
                <p class="text-sm text-gray-500 mt-1">Point your CLI tools to http://localhost:20129</p>
              </div>
            </div>
          </div>
        </div>

        <!-- Right: Code block -->
        <div class="flex-1 w-full">
          <div class="rounded-xl overflow-hidden bg-[#1e1e1e] border border-[#3a2f27] shadow-2xl">
            <!-- Terminal header -->
            <div class="flex items-center gap-2 px-4 py-3 bg-[#252526] border-b border-gray-700">
              <div class="w-3 h-3 rounded-full bg-red-500"></div>
              <div class="w-3 h-3 rounded-full bg-yellow-500"></div>
              <div class="w-3 h-3 rounded-full bg-green-500"></div>
              <div class="ml-2 text-xs text-gray-500 font-mono">terminal</div>
            </div>

            <!-- Terminal content -->
            <div class="p-6 font-mono text-sm leading-relaxed overflow-x-auto">
              <button type="button" class="flex items-center gap-2 mb-4 group cursor-pointer" @click="handleCopy('npm install -g rustrouter')">
                <span class="text-green-400">$</span>
                <span class="text-white">npm install -g rustrouter</span>
                <span class="ml-auto text-gray-500 text-xs opacity-0 group-hover:opacity-100">
                  {{ copied === "landing" ? "✓ Copied" : "Copy" }}
                </span>
              </button>

              <div class="text-gray-400 mb-6">
                <span class="text-[#f97815]">&gt;</span> <span class="text-white">rustrouter serve</span><br />
                <span class="text-gray-500">listening on <span class="text-blue-400">http://0.0.0.0:20129</span></span>
              </div>

              <div class="text-xs text-gray-500 mb-2 border-t border-gray-700 pt-4">
                📝 Configure providers in dashboard or use environment variables
              </div>

              <div class="text-gray-400 text-xs">
                <span class="text-purple-400">Data Location:</span><br />
                <span class="text-gray-500">  macOS/Linux:</span> ~/.9router/db/data.sqlite<br />
                <span class="text-gray-500">  Windows:</span> %APPDATA%/9router/db/data.sqlite
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  </section>
</template>
