<script setup lang="ts">
import { ref } from "vue";
import { useRouter } from "vue-router";

const mobileMenuOpen = ref(false);
const router = useRouter();

function goHome() {
	router.push("/");
}

function goDashboard() {
	router.push("/dashboard");
}

function toggleMobileMenu() {
	mobileMenuOpen.value = !mobileMenuOpen.value;
}

function closeMobileMenu() {
	mobileMenuOpen.value = false;
}
</script>

<template>
  <nav class="fixed top-0 z-50 w-full bg-[#181411]/80 backdrop-blur-md border-b border-[#3a2f27]">
    <div class="max-w-7xl mx-auto px-6 h-16 flex items-center justify-between">
      <button
        type="button"
        class="flex items-center gap-3 cursor-pointer bg-transparent border-none p-0"
        aria-label="Navigate to home"
        @click="goHome"
      >
        <div class="size-8 rounded bg-linear-to-br from-[#f97815] to-orange-700 flex items-center justify-center text-white">
          <span class="material-symbols-outlined text-[20px]">hub</span>
        </div>
        <h2 class="text-white text-xl font-bold tracking-tight">RustRouter</h2>
      </button>

      <div class="hidden md:flex items-center gap-8">
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors" href="#features">Features</a>
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors" href="#how-it-works">How it Works</a>
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors" href="https://github.com/walujanle/rustrouter#readme" target="_blank" rel="noopener noreferrer">Docs</a>
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors flex items-center gap-1" href="https://github.com/walujanle/rustrouter" target="_blank" rel="noopener noreferrer">
          GitHub <span class="material-symbols-outlined text-[14px]">open_in_new</span>
        </a>
      </div>

      <div class="flex items-center gap-4">
        <button
          type="button"
          class="hidden sm:flex h-9 items-center justify-center rounded-lg px-4 bg-[#f97815] hover:bg-[#e0650a] transition-all text-[#181411] text-sm font-bold shadow-[0_0_15px_rgba(249,120,21,0.4)] hover:shadow-[0_0_20px_rgba(249,120,21,0.6)]"
          @click="goDashboard"
        >
          Get Started
        </button>
        <button type="button" :aria-label="mobileMenuOpen ? 'Close menu' : 'Open menu'" :aria-expanded="mobileMenuOpen" class="md:hidden text-white" @click="toggleMobileMenu">
          <span class="material-symbols-outlined">{{ mobileMenuOpen ? "close" : "menu" }}</span>
        </button>
      </div>
    </div>

    <div v-if="mobileMenuOpen" class="md:hidden border-t border-[#3a2f27] bg-[#181411]/95 backdrop-blur-md">
      <div class="flex flex-col gap-4 p-6">
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors" href="#features" @click="closeMobileMenu">Features</a>
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors" href="#how-it-works" @click="closeMobileMenu">How it Works</a>
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors" href="https://github.com/walujanle/rustrouter#readme" target="_blank" rel="noopener noreferrer">Docs</a>
        <a class="text-gray-300 hover:text-white text-sm font-medium transition-colors" href="https://github.com/walujanle/rustrouter" target="_blank" rel="noopener noreferrer">GitHub</a>
        <button
          type="button"
          class="h-9 rounded-lg bg-[#f97815] hover:bg-[#e0650a] text-[#181411] text-sm font-bold"
          @click="goDashboard"
        >
          Get Started
        </button>
      </div>
    </div>
  </nav>
</template>
