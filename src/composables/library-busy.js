import { computed } from 'vue'
import { useDownloader } from './downloader.js'
import { useExporter } from './export.js'

// Lavender: whether a mass download or an export is writing to the library
// right now. The in-app update's Install and Restart wait for it: installing
// replaces the app on disk and restarting ends the process, and a half-written
// lyrics file or export is what an interrupted run leaves behind. A paused
// or finished download or export run is not busy: it writes nothing until
// resumed.
export function useLibraryBusy() {
  const { isActive } = useDownloader()
  const { isActive: exportActive } = useExporter()
  const libraryBusy = computed(() => isActive.value || exportActive.value)
  const busyReason = computed(() => {
    if (isActive.value) return 'Wait for the running download to finish first.'
    if (exportActive.value) return 'Wait for the running export to finish first.'
    return ''
  })
  return { libraryBusy, busyReason }
}
