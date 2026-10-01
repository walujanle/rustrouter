<script setup lang="ts">
import { onMounted, ref } from "vue";

import Card from "@/components/ui/UiCard.vue";
import Toggle from "@/components/ui/UiToggle.vue";

interface CavemanLevel {
	id: string;
	label: string;
	desc: string;
}

const CAVEMAN_LEVELS: CavemanLevel[] = [
	{ id: "lite", label: "Lite", desc: "Drop filler, keep grammar" },
	{ id: "full", label: "Full", desc: "Drop articles, fragments OK" },
	{ id: "ultra", label: "Ultra", desc: "Telegraphic, max compression" },
];

const PONYTAIL_LEVELS: CavemanLevel[] = [
	{ id: "lite", label: "Lite", desc: "Build asked, name lazier option" },
	{ id: "full", label: "Full", desc: "Ladder enforced: stdlib/native first" },
	{ id: "ultra", label: "Ultra", desc: "YAGNI extremist, deletion first" },
];

const rtkEnabled = ref(true);
const cavemanEnabled = ref(false);
const cavemanLevel = ref("full");
const ponytailEnabled = ref(false);
const ponytailLevel = ref("full");

async function patchSetting(patch: Record<string, unknown>): Promise<void> {
	try {
		await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(patch),
		});
	} catch (error) {
		console.log("Error updating setting:", error);
	}
}

async function handleRtkEnabled(value: boolean): Promise<void> {
	try {
		const res = await fetch("/api/settings", {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ rtkEnabled: value }),
		});
		if (res.ok) rtkEnabled.value = value;
	} catch (error) {
		console.log("Error updating rtkEnabled:", error);
	}
}

function handleCavemanEnabled(value: boolean): void {
	cavemanEnabled.value = value;
	patchSetting({ cavemanEnabled: value });
}

function handleCavemanLevel(level: string): void {
	cavemanLevel.value = level;
	patchSetting({ cavemanLevel: level });
}

function handlePonytailEnabled(value: boolean): void {
	ponytailEnabled.value = value;
	patchSetting({ ponytailEnabled: value });
}

function handlePonytailLevel(level: string): void {
	ponytailLevel.value = level;
	patchSetting({ ponytailLevel: level });
}

onMounted(async () => {
	try {
		const res = await fetch("/api/settings");
		if (res.ok) {
			const data = (await res.json()) as {
				rtkEnabled?: boolean;
				cavemanEnabled?: boolean;
				cavemanLevel?: string;
				ponytailEnabled?: boolean;
				ponytailLevel?: string;
			};
			rtkEnabled.value = data.rtkEnabled !== false;
			cavemanEnabled.value = !!data.cavemanEnabled;
			// A stored level the backend has no prompt for falls back to full.
			cavemanLevel.value = CAVEMAN_LEVELS.some((lvl) => lvl.id === data.cavemanLevel)
				? (data.cavemanLevel as string)
				: "full";
			ponytailEnabled.value = !!data.ponytailEnabled;
			ponytailLevel.value = data.ponytailLevel || "full";
		}
	} catch {
		// Keep the defaults when settings cannot be loaded.
	}
});
</script>

<template>
  <div class="space-y-6 p-6">
    <Card id="rtk">
      <div class="flex items-center justify-between mb-2">
        <h2 class="text-lg font-semibold flex items-center gap-2">
          <span class="material-symbols-outlined text-primary"> bolt </span>
          Token Saver
        </h2>
      </div>
      <div class="flex items-center justify-between pt-2 pb-4 border-b border-border gap-4">
        <div class="min-w-0 flex-1">
          <p class="font-medium">
            Compress tool output
            <a
              href="https://github.com/rtk-ai/rtk"
              target="_blank"
              rel="noreferrer"
              class="text-xs font-normal text-primary underline hover:opacity-80"
            >
              (RTK)
            </a>
          </p>
          <p class="text-sm text-text-muted">
            git/grep/ls/tree/logs → 60-90% fewer input tokens
          </p>
        </div>
        <Toggle
          :model-value="rtkEnabled"
          @update:model-value="handleRtkEnabled"
        />
      </div>
      <div class="flex items-center justify-between pt-4 border-t border-border gap-4 flex-wrap">
        <div class="min-w-0 flex-1">
          <p class="font-medium">
            Compress LLM output
            <a
              href="https://github.com/JuliusBrussee/caveman"
              target="_blank"
              rel="noreferrer"
              class="text-xs font-normal text-primary underline hover:opacity-80"
            >
              (Caveman)
            </a>
          </p>
          <p class="text-sm text-text-muted">
            Terse-style system prompt → ~65% fewer output tokens (up to 87%)
          </p>
        </div>
        <div class="flex items-center gap-3 shrink-0">
          <div v-if="cavemanEnabled" class="flex flex-col items-end gap-1">
            <div class="flex items-center gap-1.5">
              <button
                v-for="lvl in CAVEMAN_LEVELS"
                :key="lvl.id"
                type="button"
                :title="lvl.desc"
                :class="[
                  'px-3 py-1.5 rounded text-xs font-medium border transition-colors',
                  cavemanLevel === lvl.id
                    ? 'bg-primary text-white border-primary'
                    : 'bg-transparent border-border text-text-muted hover:bg-surface-2',
                ]"
                @click="handleCavemanLevel(lvl.id)"
              >
                {{ lvl.label }}
              </button>
            </div>
            <p class="text-xs text-primary">
              {{ CAVEMAN_LEVELS.find((lvl) => lvl.id === cavemanLevel)?.desc }}
            </p>
          </div>
          <Toggle
            :model-value="cavemanEnabled"
            @update:model-value="handleCavemanEnabled"
          />
        </div>
      </div>
      <div class="flex items-center justify-between pt-4 mt-4 border-t border-border gap-4 flex-wrap">
        <div class="min-w-0 flex-1">
          <p class="font-medium">
            Lazy senior dev
            <a
              href="https://github.com/DietrichGebert/ponytail"
              target="_blank"
              rel="noreferrer"
              class="text-xs font-normal text-primary underline hover:opacity-80"
            >
              (Ponytail)
            </a>
          </p>
          <p class="text-sm text-text-muted">
            Bias the model toward minimal code: YAGNI, reuse stdlib, deletion over addition
          </p>
        </div>
        <div class="flex items-center gap-3 shrink-0">
          <div v-if="ponytailEnabled" class="flex flex-col items-end gap-1">
            <div class="flex items-center gap-1.5">
              <button
                v-for="lvl in PONYTAIL_LEVELS"
                :key="lvl.id"
                type="button"
                :title="lvl.desc"
                :class="[
                  'px-3 py-1.5 rounded text-xs font-medium border transition-colors',
                  ponytailLevel === lvl.id
                    ? 'bg-primary text-white border-primary'
                    : 'bg-transparent border-border text-text-muted hover:bg-surface-2',
                ]"
                @click="handlePonytailLevel(lvl.id)"
              >
                {{ lvl.label }}
              </button>
            </div>
            <p class="text-xs text-primary">
              {{ PONYTAIL_LEVELS.find((lvl) => lvl.id === ponytailLevel)?.desc }}
            </p>
          </div>
          <Toggle
            :model-value="ponytailEnabled"
            @update:model-value="handlePonytailEnabled"
          />
        </div>
      </div>
    </Card>
  </div>
</template>
