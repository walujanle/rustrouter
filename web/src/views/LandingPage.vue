<script setup lang="ts">
import { useRouter } from "vue-router";
import FlowAnimation from "./landing/components/FlowAnimation.vue";
import GetStarted from "./landing/components/GetStarted.vue";
import HeroSection from "./landing/components/HeroSection.vue";
import HowItWorks from "./landing/components/HowItWorks.vue";
import Features from "./landing/components/LandingFeatures.vue";
import LandingFooter from "./landing/components/LandingFooter.vue";
import Navigation from "./landing/components/LandingNavigation.vue";

const router = useRouter();

function goDashboard() {
	router.push("/dashboard");
}

function openDocs() {
	window.open("https://github.com/walujanle/rustrouter#readme", "_blank");
}
</script>

<template>
  <div class="relative text-white font-sans overflow-x-hidden antialiased selection:bg-[#f97815] selection:text-white">
    <div class="fixed inset-0 z-0 overflow-hidden pointer-events-none bg-[#181411]">
      <div
        class="absolute inset-0 opacity-[0.06]"
        :style="{
          backgroundImage: `linear-gradient(to right, #f97815 1px, transparent 1px), linear-gradient(to bottom, #f97815 1px, transparent 1px)`,
          backgroundSize: '50px 50px',
        }"
      ></div>

      <div class="absolute top-0 left-1/4 w-175 h-175 bg-[#f97815]/12 rounded-full blur-[130px] animate-blob"></div>
      <div
        class="absolute top-1/3 right-1/4 w-150 h-150 bg-purple-500/10 rounded-full blur-[130px] animate-blob"
        :style="{ animationDelay: '2s', animationDuration: '22s' }"
      ></div>
      <div
        class="absolute bottom-0 left-1/2 w-162.5 h-162.5 bg-blue-500/8 rounded-full blur-[130px] animate-blob"
        :style="{ animationDelay: '4s', animationDuration: '25s' }"
      ></div>

      <div
        class="absolute inset-0"
        :style="{ background: 'radial-gradient(circle at center, transparent 0%, rgba(24, 20, 17, 0.4) 100%)' }"
      ></div>
    </div>

    <div class="relative z-10">
      <Navigation />

      <main>
        <div class="relative">
          <HeroSection />
          <div class="flex justify-center pb-20">
            <FlowAnimation />
          </div>
        </div>

        <GetStarted />
        <HowItWorks />
        <Features />

        <section class="py-32 px-6 relative overflow-hidden">
          <div class="absolute inset-0 bg-linear-to-t from-[#f97815]/5 to-transparent pointer-events-none"></div>
          <div class="max-w-4xl mx-auto text-center relative z-10">
            <h2 class="text-4xl md:text-5xl font-black mb-6">Point your client at one endpoint</h2>
            <p class="text-xl text-gray-400 mb-10 max-w-2xl mx-auto">
              RustRouter runs locally on port 20129, keeps your credentials in a SQLite file you own, and routes to the providers you configure. Open source, MIT licensed.
            </p>
            <div class="flex flex-col sm:flex-row items-center justify-center gap-4">
              <button
                type="button"
                class="w-full sm:w-auto h-14 px-10 rounded-lg bg-[#f97815] hover:bg-[#e0650a] text-[#181411] text-lg font-bold transition-all shadow-[0_0_20px_rgba(249,120,21,0.5)]"
                @click="goDashboard"
              >
                Open the dashboard
              </button>
              <button
                type="button"
                class="w-full sm:w-auto h-14 px-10 rounded-lg border border-[#3a2f27] hover:bg-[#23180f] text-white text-lg font-bold transition-all"
                @click="openDocs"
              >
                Read Documentation
              </button>
            </div>
          </div>
        </section>
      </main>

      <LandingFooter />
    </div>
  </div>
</template>

<style scoped>
@keyframes blob {
  0%, 100% {
    transform: translate(0, 0) scale(1);
  }
  33% {
    transform: translate(30px, -50px) scale(1.1);
  }
  66% {
    transform: translate(-20px, 20px) scale(0.9);
  }
}
.animate-blob {
  animation: blob 20s ease-in-out infinite;
}
</style>
