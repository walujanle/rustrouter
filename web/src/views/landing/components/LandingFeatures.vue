<script setup lang="ts">
const FEATURES = [
	{
		icon: "link",
		title: "One endpoint",
		desc: "Send OpenAI, Anthropic, Gemini, or Responses requests to /v1 and let the router translate to whatever the selected provider speaks.",
	},
	{
		icon: "bolt",
		title: "24 providers",
		desc: "OAuth and API-key credentials for the providers that ship in the registry, managed from the dashboard.",
	},
	{
		icon: "shield_with_heart",
		title: "Fallback",
		desc: "A failed model falls through a combo to the next one; a failed account falls through to the next connection for that provider.",
	},
	{
		icon: "monitoring",
		title: "Usage and cost",
		desc: "Token and cost accounting per provider, model, and account, written to the shared SQLite tables.",
	},
	{
		icon: "key",
		title: "Token refresh",
		desc: "OAuth tokens refresh ahead of expiry, and a connection cools down after an upstream error instead of being retried into the ground.",
	},
	{
		icon: "computer",
		title: "Local, no cloud",
		desc: "Credentials and history stay in one SQLite file on your machine. No cloud sync, no telemetry.",
	},
	{
		icon: "terminal",
		title: "CLI tools",
		desc: "Writers for Claude Code, Codex, and Hermes point each tool at the local endpoint in one step.",
	},
	{
		icon: "dashboard",
		title: "Byte-identical schema",
		desc: "The database is shared with 9router under an identical schema, so a file moves between the two installs with no migration.",
	},
];
</script>

<template>
  <section id="features" class="py-24 px-6">
    <div class="max-w-7xl mx-auto">
      <div class="mb-16">
        <h2 class="text-3xl md:text-4xl font-bold mb-4">What it does</h2>
        <p class="text-gray-400 max-w-xl text-lg">
          Everything below is local to the process: one binary, one SQLite file, one port.
        </p>
      </div>

      <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
        <div
          v-for="feature in FEATURES"
          :key="feature.title"
          class="p-6 rounded-xl bg-[#23180f] border border-[#3a2f27] transition-all duration-300 group hover:border-[#f97815]/50 hover:bg-[#f97815]/5"
        >
          <div
            class="w-10 h-10 rounded-lg flex items-center justify-center mb-4 bg-[#f97815]/10 text-[#f97815] group-hover:scale-110 transition-transform duration-300"
          >
            <span class="material-symbols-outlined">{{ feature.icon }}</span>
          </div>
          <h3 class="text-lg font-bold mb-2 transition-colors group-hover:text-[#f97815]">
            {{ feature.title }}
          </h3>
          <p class="text-sm text-gray-400 leading-relaxed">{{ feature.desc }}</p>
        </div>
      </div>
    </div>
  </section>
</template>
