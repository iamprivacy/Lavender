<template>
  <template v-if="isLocal">
    <RadioButton
      id="popup-download-upgrade"
      v-model="downloadDraft.downloadLyricsFor"
      name="popup-download-lyrics-for"
      value="upgrade"
    >
      <span class="item-label">Upgrade to the best match</span>
    </RadioButton>
    <p v-if="downloadDraft.downloadLyricsFor === 'upgrade'" class="hint">
      Plain lyrics are replaced with synced ones from this database, online LRCLIB or your extra
      sources. Synced lyrics from this database are replaced when a better match exists; lyrics you
      edited or got elsewhere stay, unless they mask words ("b***h") and the Lyrics edition setting
      wants them uncensored. Existing files next to your music are not overwritten. A stopped run
      picks up where it left off.
    </p>
  </template>
</template>

<script setup>
import { onMounted, watch } from 'vue'
import RadioButton from '@/components/common/RadioButton.vue'
import { useLyricsSource } from '@/composables/lyrics-source.js'
import { useDownloadOptions } from '@/composables/download-options.js'
import {
  isUpgradeMode,
  rememberUpgrade,
  upgradeRemembered,
} from '@/composables/upgrade-downloads.js'

// Lavender: the fourth "Download lyrics for" choice. Shown only in Local
// mode; the backend refuses it elsewhere. The choice is remembered on this
// machine, since upstream's settings only know the three skip flags.
const { isLocal, loadLyricsSource } = useLyricsSource()
const { downloadDraft, downloadLoading, request } = useDownloadOptions()

// The popup fills the draft from the saved settings after this mounts, and
// that fill replaces the whole draft. So the remembered choice is applied
// once the source is known and the fill is done, whichever comes last, and
// a change the fill makes is not taken for the user's.
let sourceKnown = false

const applyRemembered = () => {
  if (!sourceKnown || downloadLoading.value) return
  if (isLocal.value && upgradeRemembered()) {
    downloadDraft.value.downloadLyricsFor = 'upgrade'
  } else if (!isLocal.value && isUpgradeMode(downloadDraft.value.downloadLyricsFor)) {
    downloadDraft.value.downloadLyricsFor = 'all'
  }
}

onMounted(async () => {
  try {
    await loadLyricsSource()
  } catch {
    return
  }
  sourceKnown = true
  applyRemembered()
})

watch(downloadLoading, loading => {
  if (!loading) applyRemembered()
})

// Synchronous, so a change is judged while the popup's own state still says
// where it came from: the fill (still loading) and the reset on close (no
// request any more) are not the user's choice.
watch(
  () => downloadDraft.value.downloadLyricsFor,
  mode => {
    if (downloadLoading.value || !request.value) return
    rememberUpgrade(isUpgradeMode(mode))
  },
  { flush: 'sync' }
)
</script>

<style scoped>
.item-label {
  @apply text-neutral-800 dark:text-neutral-300 text-sm font-bold;
}

.hint {
  @apply px-2 text-xs text-neutral-500 dark:text-neutral-400;
}
</style>
