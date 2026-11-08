import { computed, markRaw, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

// Lavender: the mass download is a run the backend owns (download_run.rs).
// Upstream kept the queue, the counts and the log in this module, so a page
// reload (the webview's error page, a crashed renderer) wiped the run and
// a closed app forgot it. Here the module only mirrors the backend: it asks
// for a snapshot when it attaches, listens to the `download-run` event, and
// sends the user's actions as commands. The names upstream's components use
// (`addToQueue`, `downloadNext`, `startOver`, `stopDownloading`, the counts)
// are kept, so those components change as little as possible.

export const LOG_LINES = 100

const emptyRun = (revision = 0) => ({
  revision,
  exists: false,
  status: 'none',
  total: 0,
  done: 0,
  success: 0,
  failure: 0,
  kept: 0,
  skipped: 0,
  pauseReason: null,
  waiting: null,
})

const run = ref(emptyRun())
const log = ref([])
// Set when the backend reports a run the last process left unfinished, so
// the library can show the dialog that offers to resume it.
const needsAttention = ref(false)
let attaching = null

// Commands answer and the worker emits in no fixed order, so anything
// older than what is held is ignored: the backend counts a revision on
// every change, including the clearing of a run. A snapshot is the whole
// state read from the database, so one of the held revision is taken too
// (it may hold a log the events did not carry); an event or a command's
// answer only when newer.
const olderThanHeld = summary => (summary?.revision ?? 0) < (run.value.revision ?? 0)
const notNewerThanHeld = summary => (summary?.revision ?? 0) <= (run.value.revision ?? 0)

const applySnapshot = snapshot => {
  if (!snapshot?.run || olderThanHeld(snapshot.run)) return
  run.value = snapshot.run
  log.value = (snapshot.log ?? []).map(entry => markRaw({ ...entry }))
}

const applySummary = summary => {
  if (!summary || notNewerThanHeld(summary)) return false
  run.value = summary
  if (!summary.exists) log.value = []
  return true
}

const applyEvent = payload => {
  if (!payload?.run || !applySummary(payload.run)) return
  const entry = payload.entry
  if (!entry || !payload.run.exists) return
  // A track queued again (Retry not found) keeps its id: the new outcome
  // replaces the old line.
  const known = log.value.findIndex(item => item.id === entry.id)
  if (known >= 0) log.value.splice(known, 1)
  log.value.unshift(markRaw({ ...entry }))
  if (log.value.length > LOG_LINES) {
    log.value.pop()
  }
}

const refresh = async () => {
  try {
    applySnapshot(await invoke('get_download_run'))
  } catch (error) {
    console.warn('The download run could not be read:', error)
  }
}

// Attaches to the backend's run: the event first, then the snapshot, so
// nothing announced in between is missed (entries are keyed by id). Called
// once by App.vue at boot, under the name upstream used for its queue loop.
const downloadNext = () => {
  if (!attaching) {
    attaching = (async () => {
      try {
        await listen('download-run', event => applyEvent(event.payload))
      } catch (error) {
        console.warn('The download run cannot be followed:', error)
      }
      await refresh()
      if (run.value.exists && run.value.pauseReason === 'interrupted') {
        needsAttention.value = true
      }
    })()
  }
  return attaching
}

const isDownloading = computed(() => run.value.exists)
// Running right now, as opposed to paused or finished: what the in-app
// update waits for.
const isActive = computed(() => run.value.exists && run.value.status === 'running')
const isPaused = computed(() => run.value.exists && run.value.status === 'paused')
const isInterrupted = computed(() => isPaused.value && run.value.pauseReason === 'interrupted')
const isFinished = computed(() => run.value.exists && run.value.status === 'finished')
const waiting = computed(() => run.value.waiting)
const successCount = computed(() => run.value.success)
const failureCount = computed(() => run.value.failure)
const keptCount = computed(() => run.value.kept)
const skippedCount = computed(() => run.value.skipped)
const totalCount = computed(() => run.value.total)
const downloadedCount = computed(() => run.value.done)

const downloadProgress = computed(() => {
  if (!run.value.exists || run.value.total === 0) {
    return 0.0
  }
  if (run.value.done >= run.value.total) {
    return 1.0
  }
  return run.value.done / run.value.total
})

// Queues `trackIds` at the end of the run (creating it when there is none)
// and starts the backend's worker. `skipped` is how many tracks the memory
// left out before these were queued.
const addToQueue = async (trackIds, autoExport = null, upgrade = false, skipped = 0) => {
  const snapshot = await invoke('start_download_run', {
    trackIds,
    autoExport: autoExport ? { ...autoExport } : null,
    ...(upgrade ? { upgrade: true } : {}),
    ...(skipped > 0 ? { skipped } : {}),
  })
  applySnapshot(snapshot)
  console.log(`Added ${trackIds.length} tracks to download queue`)
}

const act = async (command, fallback) => {
  try {
    applySummary(await invoke(command))
  } catch (error) {
    console.warn(`${command} failed:`, error)
    await fallback?.()
  }
}

const pauseDownloading = () => act('pause_download_run', refresh)
const resumeDownloading = () => act('resume_download_run', refresh)
// The requeued tracks leave the log at once: the snapshot has them pending.
const retryFailed = () => act('retry_failed_downloads').then(refresh)

// Forgets the run: Finish once it is done, Stop while it runs, Discard for
// an interrupted one. Pause is the way to keep it for later. The revision
// is kept, so a stale event of the old run cannot bring it back; the
// backend's own clearing counts a revision above it.
const startOver = async () => {
  run.value = emptyRun(run.value.revision)
  log.value = []
  needsAttention.value = false
  try {
    applySummary(await invoke('clear_download_run'))
  } catch (error) {
    console.warn('The download run could not be cleared:', error)
    await refresh()
  }
}

const stopDownloading = () => startOver()

export function useDownloader() {
  return {
    run,
    isDownloading,
    isActive,
    isPaused,
    isInterrupted,
    isFinished,
    waiting,
    needsAttention,
    downloadProgress,
    successCount,
    failureCount,
    keptCount,
    skippedCount,
    totalCount,
    downloadedCount,
    log,
    addToQueue,
    pauseDownloading,
    resumeDownloading,
    retryFailed,
    startOver,
    stopDownloading,
    downloadNext,
    refresh,
  }
}
