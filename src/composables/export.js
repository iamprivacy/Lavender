import { computed, markRaw, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

// Lavender: the mass export is a run the backend owns (export_run.rs).
// Upstream kept the queue, the counts and the log in this module and ran
// the loop in the page, so a page reload (the webview's error page, a
// crashed renderer) wiped the run, and starting it again wrote every file
// once more. Here the module only mirrors the backend, as downloader.js
// does for the download run: it asks for a snapshot when it attaches,
// listens to the `export-run` event, and sends the user's actions as
// commands. The names upstream's components use (`addToQueue`,
// `exportNext`, `startOver`, `stopExporting`, the counts) are kept.

export const LOG_LINES = 100

const emptyRun = (revision = 0) => ({
  revision,
  exists: false,
  status: 'none',
  total: 0,
  done: 0,
  exported: 0,
  unchanged: 0,
  skipped: 0,
  errors: 0,
  pauseReason: null,
})

const run = ref(emptyRun())
const log = ref([])
// Set when the backend reports a run the last process left unfinished, so
// the library can show the dialog that offers to resume it.
const needsAttention = ref(false)
let attaching = null

// Commands answer and the worker emits in no fixed order, so anything
// older than what is held is ignored (see downloader.js).
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

// An event carries the tracks processed since the last one, oldest first.
const applyEvent = payload => {
  if (!payload?.run || !applySummary(payload.run)) return
  if (!payload.run.exists) return
  for (const entry of payload.entries ?? []) {
    // A track queued again (Retry errors) keeps its id: the new outcome
    // replaces the old line.
    const known = log.value.findIndex(item => item.id === entry.id)
    if (known >= 0) log.value.splice(known, 1)
    log.value.unshift(markRaw({ ...entry }))
  }
  if (log.value.length > LOG_LINES) {
    log.value.splice(LOG_LINES)
  }
}

const refresh = async () => {
  try {
    applySnapshot(await invoke('get_export_run'))
  } catch (error) {
    console.warn('The export run could not be read:', error)
  }
}

// Attaches to the backend's run: the event first, then the snapshot, so
// nothing announced in between is missed. Called once by App.vue at boot,
// under the name upstream used for its queue loop.
const exportNext = () => {
  if (!attaching) {
    attaching = (async () => {
      try {
        await listen('export-run', event => applyEvent(event.payload))
      } catch (error) {
        console.warn('The export run cannot be followed:', error)
      }
      await refresh()
      if (run.value.exists && run.value.pauseReason === 'interrupted') {
        needsAttention.value = true
      }
    })()
  }
  return attaching
}

const isExporting = computed(() => run.value.exists)
// Writing right now, as opposed to paused or finished: what the in-app
// update waits for.
const isActive = computed(() => run.value.exists && run.value.status === 'running')
const isPaused = computed(() => run.value.exists && run.value.status === 'paused')
const isInterrupted = computed(() => isPaused.value && run.value.pauseReason === 'interrupted')
const isFinished = computed(() => run.value.exists && run.value.status === 'finished')
const exportedCount = computed(() => run.value.exported)
const unchangedCount = computed(() => run.value.unchanged)
const skippedCount = computed(() => run.value.skipped)
const errorCount = computed(() => run.value.errors)
const totalCount = computed(() => run.value.total)
const doneCount = computed(() => run.value.done)

const exportProgress = computed(() => {
  if (!run.value.exists || run.value.total === 0) {
    return 0.0
  }
  if (run.value.done >= run.value.total) {
    return 1.0
  }
  return run.value.done / run.value.total
})

// Queues `trackIds` at the end of the run (creating it when there is none)
// with the formats chosen now, in writer order; later submissions cannot
// change a queued batch.
const addToQueue = async (trackIds, formats) => {
  const tokens = [
    ...(formats.plainText ? ['txt'] : []),
    ...(formats.syncedLrc ? ['lrc'] : []),
    ...(formats.embedIntoTrack ? ['embedded'] : []),
  ]
  const snapshot = await invoke('start_export_run', { trackIds, formats: tokens })
  applySnapshot(snapshot)
  console.log(`Added ${trackIds.length} tracks to export queue`)
}

const act = async (command, fallback) => {
  try {
    applySummary(await invoke(command))
  } catch (error) {
    console.warn(`${command} failed:`, error)
    await fallback?.()
  }
}

// The worker's last log lines may arrive under the pause's own revision,
// which the event path ignores, so the log is read again after the pause.
const pauseExporting = () => act('pause_export_run').then(refresh)
const resumeExporting = () => act('resume_export_run', refresh)
// The requeued tracks leave the log at once: the snapshot has them pending.
const retryErrors = () => act('retry_export_errors').then(refresh)

// Forgets the run: Finish once it is done, Stop while it runs, Discard for
// a paused one. The revision is kept, so a stale event of the old run
// cannot bring it back.
const startOver = async () => {
  run.value = emptyRun(run.value.revision)
  log.value = []
  needsAttention.value = false
  try {
    applySummary(await invoke('clear_export_run'))
  } catch (error) {
    console.warn('The export run could not be cleared:', error)
    await refresh()
  }
}

const stopExporting = () => startOver()

export function useExporter() {
  return {
    run,
    isExporting,
    isActive,
    isPaused,
    isInterrupted,
    isFinished,
    needsAttention,
    exportProgress,
    exportedCount,
    unchangedCount,
    skippedCount,
    errorCount,
    totalCount,
    doneCount,
    log,
    addToQueue,
    pauseExporting,
    resumeExporting,
    retryErrors,
    startOver,
    stopExporting,
    exportNext,
    refresh,
  }
}
