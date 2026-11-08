<template>
  <Transition name="slide-fade">
    <div
      v-if="toast"
      class="fixed bottom-24 right-4 z-50 w-80 rounded-lg shadow-lg shadow-neutral-900/10 dark:shadow-black/50 bg-white dark:bg-neutral-800 text-neutral-900 dark:text-neutral-100 p-4 flex flex-col gap-3 text-sm"
      role="status"
    >
      <div class="flex items-start gap-2">
        <Update class="text-lg text-hoa-1100 dark:text-hoa-400 flex-none mt-0.5" />
        <div class="grow font-bold">{{ title }}</div>
        <button
          v-if="!busy && !ready"
          class="button-tiny p-1 -mt-1 -mr-1"
          title="Close"
          @click="dismissToast"
        >
          <Close />
        </button>
      </div>

      <div
        v-if="detail"
        :class="
          failed ? 'text-red-700 dark:text-red-400' : 'text-neutral-600 dark:text-neutral-400'
        "
      >
        {{ detail }}
      </div>

      <div
        v-if="busy"
        class="h-1.5 w-full rounded-full bg-neutral-200 dark:bg-neutral-700 overflow-hidden"
      >
        <div
          class="h-full bg-hoa-1100 transition-all"
          :class="percent === null ? 'w-1/3 animate-pulse' : ''"
          :style="percent === null ? {} : { width: `${percent}%` }"
        />
      </div>

      <div class="flex justify-end gap-2">
        <template v-if="ready">
          <button class="button button-normal px-4 h-8 rounded-full text-xs" @click="hideToast">
            Later
          </button>
          <button
            class="button button-primary px-4 h-8 rounded-full text-xs"
            :disabled="libraryBusy"
            :title="libraryBusy ? busyReason : ''"
            @click="restart"
          >
            Restart now
          </button>
        </template>
        <button
          v-else-if="busy"
          class="button button-normal px-4 h-8 rounded-full text-xs"
          :disabled="progress?.phase === 'installing'"
          @click="cancel"
        >
          Cancel
        </button>
        <template v-else>
          <a
            v-if="!check?.canInstall"
            :href="check?.releaseUrl"
            target="_blank"
            class="button button-primary px-4 h-8 rounded-full text-xs"
            >View</a
          >
          <button
            v-else
            class="button button-primary px-4 h-8 rounded-full text-xs"
            :disabled="libraryBusy"
            :title="libraryBusy ? busyReason : ''"
            @click="install"
          >
            Install
          </button>
        </template>
      </div>
    </div>
  </Transition>
</template>

<script setup>
import Close from '~icons/mdi/close'
import Update from '~icons/mdi/update'
import { computed } from 'vue'
import { useAppUpdate, phaseText, progressPercent } from '@/composables/app-update.js'
import { useLibraryBusy } from '@/composables/library-busy.js'

const { toast, check, progress, busy, ready, install, cancel, restart, dismissToast, hideToast } =
  useAppUpdate()
// Installing replaces the app and restarting ends it: not while lyrics are
// being downloaded or files written.
const { libraryBusy, busyReason } = useLibraryBusy()

const percent = computed(() => progressPercent(progress.value))
const failed = computed(() => ['failed', 'cancelled'].includes(progress.value?.phase))

const title = computed(() => {
  const version = progress.value?.version || toast.value?.version
  if (ready.value) {
    return `Lavender ${version} is ready`
  }
  if (busy.value) {
    return `Updating to Lavender ${version}`
  }
  return `Lavender ${toast.value?.version} is available`
})

const detail = computed(() => {
  if (busy.value || ready.value || failed.value) {
    return phaseText(progress.value)
  }
  return check.value?.canInstall ? '' : check.value?.installBlock
})
</script>
