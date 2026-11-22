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
        <span>{{ exportedCount }} EXPORTED</span>
        <span
          v-if="unchangedCount"
          title="The file already held these lyrics, so it was left as it was"
          >{{ unchangedCount }} UNCHANGED</span
        >
        <span>{{ skippedCount }} SKIPPED</span>
        <span>{{ errorCount }} ERRORS</span>
        <span>{{ totalCount }} TOTAL</span>
      </div>
      <p v-if="notice" class="notice">{{ notice }}</p>
    </div>

    <div class="rounded-lg p-3 bg-white dark:bg-neutral-950 w-full text-xs grow overflow-auto">
      <div
        v-for="logItem in log"
        :key="logItem.id"
        class="mb-1"
        :class="{
          'text-green-800 dark:text-green-400': logItem.status === 'exported',
          'text-neutral-600 dark:text-neutral-400': logItem.status === 'unchanged',
          'text-yellow-800 dark:text-yellow-400': logItem.status === 'skipped',
          'text-red-800 dark:text-red-400': logItem.status === 'error',
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
            v-if="errorCount > 0"
            class="button button-normal px-8 py-2 rounded-full"
            title="Queues the tracks that failed once more"
            @click="retryErrors"
          >
            Retry errors
          </button>
          <button class="button button-primary px-8 py-2 rounded-full" @click="checkAndClose">
            Finish
          </button>
        </template>
        <template v-else-if="isPaused">
          <button
            class="button button-normal px-8 py-2 rounded-full"
            title="Forgets this run; the files already written stay"
            @click="handleStop"
          >
            Discard
          </button>
          <button class="button button-primary px-8 py-2 rounded-full" @click="resumeExporting">
            Resume
          </button>
        </template>
        <template v-else>
          <button
            class="button button-normal px-8 py-2 rounded-full"
            title="Keeps the run so it can be resumed later, even after a restart"
            @click="pauseExporting"
          >
            Pause
          </button>
          <button
            class="button button-normal px-8 py-2 rounded-full"
            title="Ends the run and forgets its progress; the files already written stay"
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
import { useExporter } from '@/composables/export.js'

// Lavender: the dialog mirrors the backend's export run (see export.js). It
// can be paused and resumed, survives a reload and a restart (an
// interrupted run is offered again), and can queue the tracks that failed
// once more.
const {
  run,
  exportProgress,
  exportedCount,
  unchangedCount,
  skippedCount,
  errorCount,
  totalCount,
  doneCount,
  isPaused,
  isInterrupted,
  isFinished,
  log,
  startOver,
  stopExporting,
  pauseExporting,
  resumeExporting,
  retryErrors,
} = useExporter()

const emit = defineEmits(['close'])

const progressWidth = computed(() => {
  if (exportProgress.value >= 1.0) {
    return '100%'
  }

  return `${exportProgress.value * 100}%`
})

const title = computed(() => {
  if (isFinished.value) return 'Export Complete'
  if (isInterrupted.value) return 'Export interrupted'
  if (isPaused.value) return 'Export paused'
  return 'Exporting Lyrics'
})

const left = computed(() => Math.max(totalCount.value - doneCount.value, 0))

const notice = computed(() => {
  if (isInterrupted.value) {
    return `Lavender was closed before this export finished. ${doneCount.value.toLocaleString()} of ${totalCount.value.toLocaleString()} tracks are done; Resume continues with the ${left.value.toLocaleString()} left.`
  }
  if (isPaused.value && run.value.pauseReason === 'error') {
    return `The export stopped: Lavender could not read or write its own database. ${left.value.toLocaleString()} tracks are left; Resume tries again.`
  }
  if (isPaused.value) {
    return `Paused with ${left.value.toLocaleString()} tracks left. The export is kept until it is resumed or discarded, even across a restart.`
  }
  return ''
})

const handleStop = () => {
  stopExporting()
  emit('close')
}

// Closing a finished export forgets it, as upstream's Finish did; closing a
// running or paused one leaves it, and the header button reopens it.
const checkAndClose = () => {
  if (isFinished.value) {
    startOver()
  }
  emit('close')
}

onUnmounted(() => {
  if (isFinished.value) {
    startOver()
  }
})
</script>

<style scoped>
.notice {
  @apply text-xs text-center px-4 text-neutral-600 dark:text-neutral-400;
}
</style>
