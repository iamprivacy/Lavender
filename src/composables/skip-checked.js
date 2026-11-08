import { ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { useLyricsSource } from './lyrics-source.js'

// Lavender: before a mass download, drop the tracks the local database
// already failed to answer and, for the Upgrade run, the tracks it already
// looked at and kept (with the export formats the run would write). Nothing
// happens unless the lyrics source is the local database and the "skip
// checked" setting is on, so Online mode makes exactly upstream's calls.
// The backend applies the same two checks again.

// How many tracks the last filter removed when it removed every one of them,
// so the download popup can say so instead of starting an empty run.
const allSkipped = ref(0)
// Set by "Download them anyway": the next filter lets every track through.
let bypassOnce = false

export async function applySkipChecked(ids, { upgrade = false, exportFormats = [] } = {}) {
  allSkipped.value = 0
  if (!Array.isArray(ids) || ids.length === 0) return ids
  if (bypassOnce) {
    bypassOnce = false
    return ids
  }
  const { forkConfig, isLocal } = useLyricsSource()
  if (!isLocal.value || forkConfig.value.skipChecked === false) return ids
  let kept
  try {
    kept = await invoke('filter_checked_track_ids', {
      trackIds: ids,
      ...(upgrade ? { upgrade: true, exportFormats } : {}),
    })
  } catch (error) {
    // The filter only saves time; a failing one must not stop the download.
    console.warn('Skipping checked tracks failed, downloading all of them:', error)
    return ids
  }
  if (!Array.isArray(kept)) return ids
  if (kept.length === 0) allSkipped.value = ids.length
  return kept
}

// True when the last filter removed every track; the caller then keeps the
// popup open rather than queueing nothing.
export function skippedEverything() {
  return allSkipped.value > 0
}

export function useSkipChecked() {
  const downloadAnyway = () => {
    bypassOnce = true
    allSkipped.value = 0
  }
  const dismiss = () => {
    allSkipped.value = 0
  }
  return { allSkipped, downloadAnyway, dismiss }
}
