<template>
  <template v-if="isLocal">
    <hr class="my-2 border-neutral-200 dark:border-neutral-700" />
    <fieldset :disabled="disabled">
      <legend class="section-label">Local database</legend>
      <label class="item">
        <CheckboxButton id="popup-skip-checked" v-model="skipChecked" name="popup-skip-checked">
          <span class="item-label">
            Skip tracks already checked<template v-if="count > 0">
              ({{ count.toLocaleString() }} remembered)</template
            >
          </span>
        </CheckboxButton>
      </label>
      <p class="hint">
        They are checked again when their lyrics, tags, the database or the settings change. Online
        lookups are retried after 14 days.
      </p>
      <div v-if="allSkipped > 0" class="notice">
        <span>
          All {{ allSkipped.toLocaleString() }} tracks were already checked, so there is nothing new
          to look up.
        </span>
        <button class="link text-left" type="button" @click="downloadAnyway">
          Download them anyway
        </button>
      </div>
    </fieldset>
  </template>
</template>

<script setup>
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import CheckboxButton from '@/components/common/CheckboxButton.vue'
import { useLyricsSource } from '@/composables/lyrics-source.js'
import { useSkipChecked } from '@/composables/skip-checked.js'
import { useDownloadOptions } from '@/composables/download-options.js'

// Lavender: the mass download option that leaves out tracks the current
// local database already failed to answer. Shown only in Local mode.
defineProps({ disabled: { type: Boolean, default: false } })

const { forkConfig, isLocal, loadLyricsSource, saveForkConfig } = useLyricsSource()
const count = ref(0)
const { allSkipped, downloadAnyway: bypassFilter, dismiss } = useSkipChecked()
const { downloadAllLyrics } = useDownloadOptions()

// The filter removed every track: run the same download once without it.
const downloadAnyway = () => {
  bypassFilter()
  downloadAllLyrics()
}

const skipChecked = computed({
  get: () => forkConfig.value.skipChecked !== false,
  set: async value => {
    await saveForkConfig({ ...forkConfig.value, skipChecked: value })
  },
})

onUnmounted(dismiss)

onMounted(async () => {
  dismiss()
  try {
    await loadLyricsSource()
    if (isLocal.value) {
      const checked = await invoke('get_checked_count')
      count.value = checked.count + (checked.upgradeCount ?? 0)
    }
  } catch {
    count.value = 0
  }
})
</script>

<style scoped>
.item {
  @apply flex items-center px-2 py-1 hover:bg-neutral-100 dark:hover:bg-neutral-700 rounded cursor-pointer gap-1 w-full;
}

.item-label {
  @apply text-neutral-800 dark:text-neutral-300 text-sm font-bold;
}

.hint {
  @apply px-2 text-xs text-neutral-500 dark:text-neutral-400;
}

.notice {
  @apply flex flex-col gap-1 px-2 py-1 text-xs text-neutral-700 dark:text-neutral-300;
}

.section-label {
  @apply text-xs uppercase font-bold text-neutral-900 dark:text-neutral-400 px-2 py-1;
}
</style>
