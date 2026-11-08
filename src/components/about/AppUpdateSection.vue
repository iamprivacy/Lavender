<template>
  <div>
    <label class="group-label mb-2">Update</label>

    <div class="text-sm flex flex-col gap-2">
      <div>
        Current version: <span class="font-bold">{{ info?.currentVersion || '...' }}</span>
      </div>

      <template v-if="check && check.status !== 'failed'">
        <div>
          Newest version: <span class="font-bold">{{ check.latestVersion }}</span>
        </div>
        <div
          v-if="check.status === 'available'"
          class="font-bold text-yellow-600 flex flex-wrap gap-1 items-center"
        >
          <Alert class="text-lg" />
          Lavender {{ check.latestVersion }} is available. See what's new on the
          <span><a :href="check.releaseUrl" class="link" target="_blank">release page</a>.</span>
        </div>
        <div v-else class="font-bold text-green-600 flex flex-wrap gap-1 items-center">
          <CheckCircle class="text-lg" />
          You are using the newest version.
        </div>
      </template>
      <div v-else-if="check" class="text-red-700 dark:text-red-400">
        Could not check for updates: {{ check.error }}
      </div>

      <div v-if="busy || progress?.phase === 'ready' || progressFailed" class="flex flex-col gap-2">
        <div :class="progressFailed ? 'text-red-700 dark:text-red-400' : ''">
          {{ statusText }}
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
      </div>

      <div class="flex flex-wrap gap-2 items-center">
        <button
          v-if="ready"
          class="button button-primary px-4 h-8 rounded-full text-xs"
          :disabled="libraryBusy"
          :title="libraryBusy ? busyReason : ''"
          @click="restart"
        >
          Restart now
        </button>
        <button
          v-else-if="busy"
          class="button button-normal px-4 h-8 rounded-full text-xs"
          :disabled="progress?.phase === 'installing'"
          @click="cancel"
        >
          Cancel
        </button>
        <template v-else>
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs"
            :disabled="checking"
            @click="checkNow"
          >
            {{ checking ? 'Checking...' : 'Check for updates' }}
          </button>
          <button
            v-if="check?.canInstall"
            class="button button-primary px-4 h-8 rounded-full text-xs"
            :disabled="libraryBusy"
            :title="libraryBusy ? busyReason : ''"
            @click="install"
          >
            Install update
          </button>
        </template>
        <span v-if="libraryBusy" class="text-xs text-neutral-500 dark:text-neutral-400">
          {{ busyReason }}
        </span>
      </div>

      <div
        v-if="check?.status === 'available' && !check.canInstall && check.installBlock"
        class="text-xs text-neutral-500 dark:text-neutral-400"
      >
        {{ check.installBlock }} Download it from the
        <a :href="check.releaseUrl" class="link text-xs" target="_blank">Lavender Releases page</a>.
      </div>

      <div class="flex flex-col gap-2 mt-2">
        <CheckboxButton
          id="app-update-auto-check"
          :model-value="autoCheck"
          name="app-update-auto-check"
          @update:model-value="saveAutoCheck"
        >
          Check for updates automatically
        </CheckboxButton>
        <div v-if="autoCheck" class="flex items-center gap-2 pl-7">
          <label class="child-label" for="app-update-cadence">How often</label>
          <select
            id="app-update-cadence"
            :value="cadence"
            class="select select-xs"
            @change="saveCadence($event.target.value)"
          >
            <option value="launch">Every launch</option>
            <option value="daily">Once a day</option>
          </select>
        </div>
        <span class="text-xs text-neutral-500 dark:text-neutral-400">
          A check only reads the latest release of github.com/iamprivacy/Lavender. Nothing is
          downloaded unless you choose Install, and an update is installed only after its signed
          checksums match.
        </span>
      </div>
    </div>
  </div>
</template>

<script setup>
import Alert from '~icons/mdi/alert'
import CheckCircle from '~icons/mdi/check-circle'
import { computed, onMounted } from 'vue'
import CheckboxButton from '@/components/common/CheckboxButton.vue'
import { useAppUpdate, phaseText, progressPercent } from '@/composables/app-update.js'
import { useLibraryBusy } from '@/composables/library-busy.js'

// Installing replaces the app and restarting ends it: not while lyrics are
// being downloaded or files written.
const { libraryBusy, busyReason } = useLibraryBusy()

const {
  info,
  check,
  checking,
  progress,
  busy,
  ready,
  loadInfo,
  checkNow,
  setPrefs,
  install,
  cancel,
  restart,
} = useAppUpdate()

const autoCheck = computed(() => info.value?.prefs?.autoCheck ?? false)
const cadence = computed(() => info.value?.prefs?.cadence ?? 'daily')
const percent = computed(() => progressPercent(progress.value))
const progressFailed = computed(() => ['failed', 'cancelled'].includes(progress.value?.phase))

const statusText = computed(() => phaseText(progress.value))

const saveAutoCheck = async value => {
  await setPrefs({ autoCheck: value, cadence: cadence.value })
}

const saveCadence = async value => {
  await setPrefs({ autoCheck: autoCheck.value, cadence: value })
}

onMounted(() => {
  loadInfo().catch(console.error)
})
</script>
