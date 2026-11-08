<template>
  <BaseModal
    content-class="w-full h-[80vh] max-w-screen-md"
    body-class="flex flex-col h-full min-h-0 justify-between gap-6"
    :title="title"
    @close="checkAndClose"
  >
    <div class="flex flex-col items-center justify-center gap-1">
      <div class="w-full bg-neutral-50 h-1 rounded">
        <div class="bg-hoa-1100 h-1" :style="{ width: progressWidth }" />
      </div>
      <div class="text-[0.7rem] text-neutral-500 dark:text-neutral-500 flex gap-3">
        <span>{{ successCount }} FOUND</span>
        <span>{{ failureCount }} FAILED</span>
        <span v-if="keptCount">{{ keptCount }} KEPT</span>
        <span
          v-if="skippedCount"
          title="Left out: remembered as not found in this database, or already checked by Upgrade"
          >{{ skippedCount }} SKIPPED</span
        >
        <span>{{ totalCount }} TOTAL</span>
      </div>
      <p v-if="notice" class="notice" :class="noticeClass">{{ notice }}</p>
    </div>

    <div class="rounded-lg p-3 bg-white dark:bg-neutral-950 w-full text-xs grow overflow-auto">
      <div
        v-for="logItem in log"
        :key="logItem.id"
        :class="{
          'text-green-800 dark:text-green-400': logItem.status === 'success',
          'text-red-800 dark:text-red-400': logItem.status === 'failure',
          'text-neutral-600 dark:text-neutral-400': logItem.status === 'kept',
        }"
      >
        <strong>{{ logItem.title }} - {{ logItem.artistName }}</strong
        >:
        <span>{{ logItem.message }}</span>
      </div>
    </div>

    <template #footer>
      <div class="flex-none flex justify-center gap-3">
        <template v-if="isFinished">
          <button
            v-if="failureCount > 0"
            class="button button-normal px-8 py-2 rounded-full"
            title="Queues the tracks that were not found or failed once more"
            @click="retryFailed"
          >
            Retry not found
          </button>
          <button class="button button-primary px-8 py-2 rounded-full" @click="checkAndClose">
            Finish
          </button>
        </template>
        <template v-else-if="isPaused">
          <button
            class="button button-normal px-8 py-2 rounded-full"
            title="Forgets this run; the lyrics already saved stay"
            @click="handleStop"
          >
            Discard
          </button>
          <button class="button button-primary px-8 py-2 rounded-full" @click="resumeDownloading">
            Resume
          </button>
        </template>
        <template v-else>
          <button
            class="button button-normal px-8 py-2 rounded-full"
            title="Keeps the run so it can be resumed later, even after a restart"
            @click="pauseDownloading"
          >
            Pause
          </button>
          <button
            class="button button-normal px-8 py-2 rounded-full"
            title="Ends the run and forgets its progress; the lyrics already saved stay"
            @click="handleStop"
          >
            Stop
          </button>
        </template>
      </div>
    </template>
  </BaseModal>
</template>

<script setup>
import { onUnmounted, computed } from 'vue'
import { useDownloader } from '@/composables/downloader.js'

// Lavender: the dialog mirrors the backend's run (see downloader.js). It can
// be paused and resumed, survives a reload and a restart (an interrupted run
// is offered again), waits out a lost connection, and can queue the tracks
// that were not found once more.
const {
  run,
  downloadProgress,
  successCount,
  failureCount,
  keptCount,
  skippedCount,
  totalCount,
  downloadedCount,
  isPaused,
  isInterrupted,
  isFinished,
  waiting,
  startOver,
  stopDownloading,
  pauseDownloading,
  resumeDownloading,
  retryFailed,
  log,
} = useDownloader()

const emit = defineEmits(['close'])

const progressWidth = computed(() => {
  if (downloadProgress.value > 1.0) {
    return '100%'
  }

  return `${downloadProgress.value * 100}%`
})

const title = computed(() => {
  if (isFinished.value) return 'Downloaded'
  if (isInterrupted.value) return 'Download interrupted'
  if (isPaused.value) return 'Download paused'
  return 'Downloading'
})

const left = computed(() => Math.max(totalCount.value - downloadedCount.value, 0))

const notice = computed(() => {
  if (isInterrupted.value) {
    return `Lavender was closed before this run finished. ${downloadedCount.value.toLocaleString()} of ${totalCount.value.toLocaleString()} tracks are done; Resume continues with the ${left.value.toLocaleString()} left.`
  }
  if (isPaused.value && run.value.pauseReason === 'error') {
    return `The run stopped: Lavender could not read or write its own database. ${left.value.toLocaleString()} tracks are left; Resume tries again.`
  }
  if (isPaused.value) {
    return `Paused with ${left.value.toLocaleString()} tracks left. The run is kept until it is resumed or discarded, even across a restart.`
  }
  if (waiting.value) {
    return `${waiting.value} Nothing is counted as not found while it waits.`
  }
  return ''
})

const noticeClass = computed(() => ({
  'text-amber-700 dark:text-amber-400': !!waiting.value && !isPaused.value,
  'text-neutral-600 dark:text-neutral-400': isPaused.value,
}))

const handleStop = () => {
  stopDownloading()
  emit('close')
}

const checkAndClose = () => {
  if (isFinished.value) {
    startOver()
    emit('close')
  } else {
    emit('close')
  }
}

onUnmounted(() => {
  checkAndClose()
})
</script>

<style scoped>
.notice {
  @apply text-xs text-center px-4;
}
</style>
