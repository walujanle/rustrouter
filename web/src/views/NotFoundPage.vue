<script setup lang="ts">
import { RouterLink } from "vue-router";

import Button from "@/components/ui/UiButton.vue";
import Card from "@/components/ui/UiCard.vue";
</script>

<template>
  <div class="min-h-screen flex items-center justify-center bg-bg p-6">
    <Card class-name="max-w-md w-full text-center">
      <p class="text-5xl font-semibold text-text-main mb-2">404</p>
      <h1 class="text-xl font-semibold text-text-main mb-2">Page not found</h1>
      <p class="text-text-muted mb-6">The page you are looking for does not exist.</p>
      <RouterLink to="/dashboard">
        <Button icon="arrow_back">Back to dashboard</Button>
      </RouterLink>
    </Card>
  </div>
</template>
