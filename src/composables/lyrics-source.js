import { ref, computed } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

// Lavender: shared state for the lyrics source (online LRCLIB or a local
// copy of the LRCLIB database). Module-level refs, like the other composables.

const forkConfigState = ref({
  lyricsSource: 'online',
  localDbPath: null,
  fallbackOnline: true,
  tolerantMatching: true,
  skipChecked: true,
  instrumentalTitles: true,
  extraAmll: false,
  extraApple: false,
  appleUserToken: null,
  extraNetease: false,
  extraQq: false,
  extraPlainLyrics: false,
  exportWordTimestamps: false,
  lyricsEdition: 'follow-file',
})
const localDbStatusState = ref({ configuredPath: null, open: false, info: null, error: null })
// Why the last save of these settings failed, until one succeeds. Kept here
// rather than in the settings section, which closes with the dialog.
const saveErrorState = ref(null)
let listening = null

export function useLyricsSource() {
  const forkConfig = computed(() => forkConfigState.value)
  const localDbStatus = computed(() => localDbStatusState.value)
  const lyricsSource = computed(() => forkConfigState.value.lyricsSource)
  const isLocal = computed(() => forkConfigState.value.lyricsSource === 'local')
  const saveError = computed(() => saveErrorState.value)

  const setForkConfig = config => {
    forkConfigState.value = config
  }

  const setLocalDbStatus = status => {
    localDbStatusState.value = status
  }

  // Keeps the status in sync with the backend (database opened, closed or lost).
  const ensureListening = async () => {
    if (!listening) {
      listening = listen('local-db-status', event => {
        localDbStatusState.value = event.payload
      })
    }
    return listening
  }

  const loadLyricsSource = async () => {
    await ensureListening()
    const [config, status] = await Promise.all([
      invoke('get_fork_config'),
      invoke('get_local_db_status'),
    ])
    forkConfigState.value = config
    localDbStatusState.value = status
    return { config, status }
  }

  const saveForkConfig = async config => {
    let saved
    try {
      saved = await invoke('set_fork_config', { forkConfig: config })
    } catch (error) {
      saveErrorState.value = String(error)
      throw error
    }
    saveErrorState.value = null
    forkConfigState.value = saved
    return saved
  }

  return {
    forkConfig,
    localDbStatus,
    lyricsSource,
    isLocal,
    saveError,
    setForkConfig,
    setLocalDbStatus,
    ensureListening,
    loadLyricsSource,
    saveForkConfig,
  }
}

export function humanSize(bytes) {
  if (bytes === null || bytes === undefined) return ''
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let value = Number(bytes)
  let unit = 0
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000
    unit += 1
  }
  return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`
}

export function humanSpeed(bytesPerSecond) {
  if (!bytesPerSecond || bytesPerSecond <= 0) return ''
  return `${humanSize(Math.round(bytesPerSecond))}/s`
}

// The folder part of a path, for either separator.
export function parentFolder(path) {
  if (!path) return ''
  const cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return cut > 0 ? path.slice(0, cut) : ''
}
