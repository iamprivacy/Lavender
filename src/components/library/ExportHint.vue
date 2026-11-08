<template>
  <p v-if="!downloadLoading && !downloadDraft.autoExportEnabled" class="hint">
    Other music players won't see the lyrics until they are exported. Tick this, or use Export
    later.
  </p>
</template>

<script setup>
import { useDownloadOptions } from '@/composables/download-options.js'

// Lavender: with automatic export off, a download only fills the app's
// library, and other players read lyrics only from files next to the music.
// Shown under the checkbox in both lyrics source modes; it changes no call.
// Hidden while the popup loads, when the draft still holds the defaults.
const { downloadDraft, downloadLoading } = useDownloadOptions()
</script>

<style scoped>
.hint {
  @apply px-2 pb-1 text-xs text-neutral-500 dark:text-neutral-400;
}
</style>
