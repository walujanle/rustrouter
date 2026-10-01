<template>
  <footer class="border-t border-[#3a2f27] bg-[#120f0d] pt-16 pb-8 px-6">
    <div class="max-w-7xl mx-auto">
      <div class="grid grid-cols-2 md:grid-cols-4 lg:grid-cols-5 gap-8 mb-16">
        <div class="col-span-2 lg:col-span-2">
          <div class="flex items-center gap-3 mb-6">
            <div class="size-6 rounded bg-[#f97815] flex items-center justify-center text-white">
              <span class="material-symbols-outlined text-[16px]">hub</span>
            </div>
            <h3 class="text-white text-lg font-bold">RustRouter</h3>
          </div>
          <p class="text-gray-400 text-sm max-w-xs mb-6">
            One local endpoint for your AI providers. Route, translate, and track usage without sending anything through a third party.
          </p>
          <div class="flex gap-4">
            <a class="text-gray-400 hover:text-white transition-colors" href="https://github.com/walujanle/rustrouter" target="_blank" rel="noopener noreferrer">
              <span class="material-symbols-outlined">code</span>
            </a>
          </div>
        </div>

        <div class="flex flex-col gap-4">
          <h4 class="font-bold text-white">Product</h4>
          <a class="text-gray-400 hover:text-[#f97815] text-sm transition-colors" href="#features">Features</a>
          <a class="text-gray-400 hover:text-[#f97815] text-sm transition-colors" href="/dashboard">Dashboard</a>
          <a class="text-gray-400 hover:text-[#f97815] text-sm transition-colors" href="https://github.com/walujanle/rustrouter/blob/main/CHANGELOG.md" target="_blank" rel="noopener noreferrer">Changelog</a>
        </div>

        <div class="flex flex-col gap-4">
          <h4 class="font-bold text-white">Resources</h4>
          <a class="text-gray-400 hover:text-[#f97815] text-sm transition-colors" href="https://github.com/walujanle/rustrouter#readme" target="_blank" rel="noopener noreferrer">Documentation</a>
          <a class="text-gray-400 hover:text-[#f97815] text-sm transition-colors" href="https://github.com/walujanle/rustrouter" target="_blank" rel="noopener noreferrer">GitHub</a>
          <a class="text-gray-400 hover:text-[#f97815] text-sm transition-colors" href="https://www.npmjs.com/package/rustrouter" target="_blank" rel="noopener noreferrer">NPM</a>
        </div>

        <div class="flex flex-col gap-4">
          <h4 class="font-bold text-white">Legal</h4>
          <a class="text-gray-400 hover:text-[#f97815] text-sm transition-colors" href="https://github.com/walujanle/rustrouter/blob/main/LICENSE" target="_blank" rel="noopener noreferrer">MIT License</a>
        </div>
      </div>

      <div class="border-t border-[#3a2f27] pt-8 flex flex-col md:flex-row justify-between items-center gap-4">
        <p class="text-gray-400 text-sm">© 2025 RustRouter. All rights reserved.</p>
        <div class="flex gap-6">
          <a class="text-gray-400 hover:text-white text-sm transition-colors" href="https://github.com/walujanle/rustrouter" target="_blank" rel="noopener noreferrer">GitHub</a>
          <a class="text-gray-400 hover:text-white text-sm transition-colors" href="https://www.npmjs.com/package/rustrouter" target="_blank" rel="noopener noreferrer">NPM</a>
        </div>
      </div>
    </div>
  </footer>
</template>
