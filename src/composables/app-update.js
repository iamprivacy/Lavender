import { ref, computed } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

// Lavender: in-app update state shared by the About dialog, the update toast
// and the one-time opt-in prompt. Module-level refs, like the other
// composables. The backend (src-tauri/src/app_update) does every check and
// verification; this only shows what it reports.

const infoState = ref(null)
const checkState = ref(null)
const checkingState = ref(false)
const progressState = ref(null)
const toastState = ref(null)
let listening = null

export const BUSY_PHASES = ['verifying', 'downloading', 'installing']

export function progressPercent(progress) {
  if (!progress || !progress.total) {
    return null
  }
  return Math.min(100, Math.round((progress.downloaded / progress.total) * 100))
}

export function phaseText(progress) {
  switch (progress?.phase) {
    case 'verifying':
      return 'Verifying the release...'
    case 'downloading': {
      const percent = progressPercent(progress)
      return percent === null ? 'Downloading...' : `Downloading... ${percent}%`
    }
    case 'installing':
      return 'Installing...'
    case 'ready':
      return progress.message || 'Restart to finish the update.'
    case 'failed':
      return `Install failed: ${progress.message || 'unknown error'}`
    case 'cancelled':
      return 'Install cancelled'
    default:
      return ''
  }
}

export function useAppUpdate() {
  const info = computed(() => infoState.value)
  const check = computed(() => checkState.value)
  const checking = computed(() => checkingState.value)
  const progress = computed(() => progressState.value)
  const toast = computed(() => toastState.value)
  const busy = computed(() => BUSY_PHASES.includes(progressState.value?.phase))
  const ready = computed(() => progressState.value?.phase === 'ready')

  const ensureListening = async () => {
    if (!listening) {
      listening = listen('app-update-progress', event => {
        progressState.value = event.payload
      })
    }
    return listening
  }

  const loadInfo = async () => {
    await ensureListening()
    infoState.value = await invoke('get_app_update_info')
    if (infoState.value?.progress && !progressState.value) {
      progressState.value = infoState.value.progress
    }
    return infoState.value
  }

  // Automatic checks offer the toast; a check the user asked for in About
  // does not, and neither does a release whose notice was closed before.
  const offerToast = result => {
    if (
      result?.status === 'available' &&
      !result.dismissed &&
      progressState.value?.phase !== 'ready'
    ) {
      toastState.value = { version: result.latestVersion }
    }
  }

  const checkNow = async () => {
    checkingState.value = true
    try {
      checkState.value = await invoke('check_app_update')
    } catch (error) {
      checkState.value = { status: 'failed', error: String(error) }
    } finally {
      checkingState.value = false
    }
    return checkState.value
  }

  const startupCheck = async () => {
    try {
      const result = await invoke('startup_app_update_check')
      if (result) {
        checkState.value = result
        offerToast(result)
      }
    } catch (error) {
      console.error(error)
    }
  }

  const resolveOptIn = async accept => {
    try {
      const result = await invoke('resolve_app_update_opt_in', { accept })
      if (result) {
        checkState.value = result
        offerToast(result)
      }
    } catch (error) {
      console.error(error)
    }
    await loadInfo()
  }

  const setPrefs = async ({ autoCheck, cadence }) => {
    const prefs = await invoke('set_app_update_prefs', { autoCheck, cadence })
    if (infoState.value) {
      infoState.value = { ...infoState.value, prefs, shouldPrompt: false }
    }
    return prefs
  }

  const install = async () => {
    await ensureListening()
    progressState.value = { phase: 'verifying', downloaded: 0, total: null }
    try {
      await invoke('install_app_update')
    } catch (error) {
      progressState.value = { phase: 'failed', message: String(error) }
    }
  }

  const cancel = async () => {
    try {
      await invoke('cancel_app_update_install')
    } catch (error) {
      console.error(error)
    }
  }

  const restart = async () => {
    try {
      await invoke('restart_for_app_update')
    } catch (error) {
      progressState.value = { phase: 'failed', message: String(error) }
    }
  }

  // Closing the toast's offer remembers the release; "Later" only hides it.
  const dismissToast = async () => {
    const version = toastState.value?.version
    toastState.value = null
    if (version) {
      try {
        await invoke('dismiss_app_update', { version })
      } catch (error) {
        console.error(error)
      }
    }
  }

  const hideToast = () => {
    toastState.value = null
  }

  return {
    info,
    check,
    checking,
    progress,
    toast,
    busy,
    ready,
    loadInfo,
    checkNow,
    startupCheck,
    resolveOptIn,
    setPrefs,
    install,
    cancel,
    restart,
    dismissToast,
    hideToast,
  }
}
