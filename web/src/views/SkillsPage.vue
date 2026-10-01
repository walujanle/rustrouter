<script setup lang="ts">
import Badge from "@/components/ui/UiBadge.vue";
import Card from "@/components/ui/UiCard.vue";
import { getSkillUrl, SKILLS } from "@/constants/skills";
import { useCopyToClipboard } from "@/hooks/useCopyToClipboard";

// One shared clipboard helper; `copied` holds the id of the row that was copied.
const { copied, copy } = useCopyToClipboard(2000);
</script>

<template>
  <div class="max-w-4xl mx-auto space-y-6">
    <Card padding="md">
      <div class="text-xs text-text-muted mb-2">Paste this to your AI:</div>
      <div class="px-3 py-2 rounded bg-surface-2 font-mono text-[12px] text-text-main">
        Read this skill and use it: {{ getSkillUrl("9router") }}
      </div>
    </Card>

    <div class="space-y-2">
      <div
        v-for="skill in SKILLS"
        :key="skill.id"
        :class="[
          'flex items-start gap-3 p-4 rounded-[14px] border shadow-soft transition-colors',
          skill.isEntry
            ? 'border-brand-500/40 bg-brand-500/5'
            : 'border-border-subtle bg-surface hover:bg-surface-2',
        ]"
      >
        <div
          :class="[
            'size-9 rounded-lg flex items-center justify-center shrink-0',
            skill.isEntry ? 'bg-primary text-white' : 'bg-primary/10 text-primary',
          ]"
        >
          <span class="material-symbols-outlined text-[18px]">{{ skill.icon }}</span>
        </div>

        <div class="min-w-0 flex-1">
          <div class="flex items-center gap-2 flex-wrap">
            <h3 class="font-semibold text-sm text-text-main">{{ skill.name }}</h3>
            <Badge v-if="skill.isEntry" variant="primary" size="sm">START HERE</Badge>
            <Badge v-if="skill.endpoint" variant="default" size="sm">
              <code class="text-[10px]">{{ skill.endpoint }}</code>
            </Badge>
          </div>
          <p class="text-xs text-text-muted mt-0.5">{{ skill.description }}</p>
          <a
            :href="getSkillUrl(skill.id)"
            target="_blank"
            rel="noreferrer"
            class="text-[11px] text-text-muted hover:text-primary mt-1 inline-flex items-center gap-1 break-all"
          >
            {{ getSkillUrl(skill.id) }}
            <span class="material-symbols-outlined text-[12px]">open_in_new</span>
          </a>
        </div>

        <button
          type="button"
          :title="getSkillUrl(skill.id)"
          class="px-2 py-1 rounded-md bg-primary text-white text-[11px] font-medium hover:bg-primary/90 transition-colors cursor-pointer shrink-0 inline-flex items-center gap-1"
          @click="copy(getSkillUrl(skill.id), skill.id)"
        >
          <span class="material-symbols-outlined text-[12px]">
            {{ copied === skill.id ? "check" : "content_copy" }}
          </span>
          {{ copied === skill.id ? "Copied!" : "Copy link" }}
        </button>
      </div>
    </div>
  </div>
</template>
