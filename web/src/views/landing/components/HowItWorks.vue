<template>
  <section id="how-it-works" class="py-24 border-y border-[#3a2f27] bg-[#23180f]/30">
    <div class="max-w-7xl mx-auto px-6">
      <div class="mb-16">
        <h2 class="text-3xl md:text-4xl font-bold mb-4">How RustRouter Works</h2>
        <p class="text-gray-400 max-w-xl text-lg">
          Your request goes in, RustRouter picks a provider you configured, and the response comes back in the shape your client expects.
        </p>
      </div>

      <div class="grid grid-cols-1 md:grid-cols-3 gap-8 relative">
        <!-- Connection line -->
        <div class="hidden md:block absolute top-12 left-[16%] right-[16%] h-0.5 bg-linear-to-r from-gray-700 via-[#f97815] to-gray-700 -z-10"></div>

        <!-- Step 1: CLI & SDKs -->
        <div class="flex flex-col gap-6 relative group">
          <div class="w-24 h-24 rounded-2xl bg-[#181411] border border-[#3a2f27] flex items-center justify-center shadow-xl group-hover:border-gray-500 transition-colors z-10 mx-auto md:mx-0">
            <span class="material-symbols-outlined text-4xl text-gray-300">terminal</span>
          </div>
          <div>
            <h3 class="text-xl font-bold mb-2">1. CLI &amp; SDKs</h3>
            <p class="text-sm text-gray-400">
              Your requests start from your own tools. Point them at the local base URL and nothing else changes.
            </p>
          </div>
        </div>

        <!-- Step 2: RustRouter Hub -->
        <div class="flex flex-col gap-6 relative group md:items-center md:text-center">
          <div class="w-24 h-24 rounded-2xl bg-[#181411] border-2 border-[#f97815] flex items-center justify-center shadow-[0_0_30px_rgba(249,120,21,0.2)] z-10 mx-auto">
            <span class="material-symbols-outlined text-4xl text-[#f97815] animate-pulse">hub</span>
          </div>
          <div>
            <h3 class="text-xl font-bold mb-2 text-[#f97815]">2. RustRouter Hub</h3>
            <p class="text-sm text-gray-400">
              It translates the request to the provider's format, and falls back to the next account or model when one fails.
            </p>
          </div>
        </div>

        <!-- Step 3: AI Providers -->
        <div class="flex flex-col gap-6 relative group md:items-end md:text-right">
          <div class="w-24 h-24 rounded-2xl bg-[#181411] border border-[#3a2f27] flex items-center justify-center shadow-xl group-hover:border-gray-500 transition-colors z-10 mx-auto md:mx-0">
            <div class="grid grid-cols-2 gap-2">
              <div class="w-6 h-6 rounded bg-white/10"></div>
              <div class="w-6 h-6 rounded bg-white/10"></div>
              <div class="w-6 h-6 rounded bg-white/10"></div>
              <div class="w-6 h-6 rounded bg-white/10"></div>
            </div>
          </div>
          <div>
            <h3 class="text-xl font-bold mb-2">3. AI Providers</h3>
            <p class="text-sm text-gray-400">
              The provider answers, and the response is translated back to what your client asked for.
            </p>
          </div>
        </div>
      </div>
    </div>
  </section>
</template>
