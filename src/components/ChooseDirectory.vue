<template>
  <div class="flex flex-col w-full h-full justify-center items-center">
    <div class="px-4 py-2 flex flex-col gap-4 flex-none">
      <div class="text-thin text-xl text-neutral-900 dark:text-neutral-200">Select directories</div>
    </div>

    <div class="grow flex flex-col items-center justify-center gap-8 w-full max-w-screen-sm">
      <div class="flex flex-col gap-2 w-full justify-center items-center">
        <div
          v-for="(directory, index) in directories"
          :key="directory"
          class="w-full bg-neutral-100 dark:bg-neutral-900 text-neutral-900 dark:text-neutral-200 font-bold p-4 rounded-lg flex items-center"
        >
          <div class="grow">
            {{ directory }}
          </div>
          <button
            class="flex-none button button-normal p-2 rounded-full"
            @click.prevent="removeDirectory(index)"
          >
            <Close />
          </button>
        </div>

        <button
          class="w-full bg-neutral-50 dark:bg-neutral-900 hover:bg-neutral-100 hover:dark:bg-neutral-800 active:bg-neutral-200/50 transition text-neutral-900 dark:text-neutral-300 font-bold p-4 rounded-lg flex items-center border border-dashed border-neutral-400 dark:border-neutral-600"
          @click.prevent="chooseDirectory"
        >
          <Plus />
          <div>Add new directory</div>
        </button>
      </div>

      <div v-if="savedScans.length" class="flex flex-col gap-2 w-full">
        <div class="text-sm text-neutral-600 dark:text-neutral-400">
          Saved scans of removed folders. Adding one back restores it without a rescan.
        </div>
        <div
          v-for="saved in savedScans"
          :key="saved.path"
          class="w-full bg-neutral-50 dark:bg-neutral-900 text-neutral-700 dark:text-neutral-300 p-3 rounded-lg flex items-center gap-2"
        >
          <div class="grow min-w-0">
            <div class="truncate">{{ saved.path }}</div>
            <div class="text-xs text-neutral-500">
              {{ saved.trackCount }} {{ saved.trackCount === 1 ? 'track' : 'tracks' }}
            </div>
          </div>
          <button
            class="flex-none button button-normal px-3 py-1 rounded-full text-sm"
            @click.prevent="addBack(saved.path)"
          >
            Add back
          </button>
          <button
            class="flex-none button button-normal px-3 py-1 rounded-full text-sm"
            @click.prevent="forget(saved.path)"
          >
            {{ confirmingForget === saved.path ? 'Really forget?' : 'Forget' }}
          </button>
        </div>
      </div>

      <button class="button button-primary w-full p-4 rounded-lg" @click.prevent="progressStep">
        Continue
      </button>
    </div>
  </div>
</template>

<script setup>
import { open } from '@tauri-apps/plugin-dialog'
import { audioDir } from '@tauri-apps/api/path'
import { invoke } from '@tauri-apps/api/core'
import { ref, computed, onMounted } from 'vue'
import Close from '~icons/mdi/close'
import Plus from '~icons/mdi/plus'
const emit = defineEmits(['progressStep', 'directoriesChanged'])

const directories = ref([])
const originalDirectories = ref([])
const parkedFolders = ref([])
const confirmingForget = ref(null)

// Folders shown in the list above are not offered again here
const savedScans = computed(() =>
  parkedFolders.value.filter(saved => !directories.value.includes(saved.path))
)

const loadParkedFolders = async () => {
  try {
    parkedFolders.value = await invoke('list_parked_folders')
  } catch (error) {
    console.error(error)
    parkedFolders.value = []
  }
}

const addBack = path => {
  if (!directories.value.includes(path)) {
    directories.value.push(path)
  }
}

const forget = async path => {
  if (confirmingForget.value !== path) {
    confirmingForget.value = path
    return
  }
  confirmingForget.value = null
  await invoke('forget_parked_folder', { path })
  await loadParkedFolders()
}

const progressStep = async () => {
  await invoke('set_directories', { directories: directories.value })

  // Check if directories actually changed
  const hasChanged =
    JSON.stringify(originalDirectories.value.sort()) !== JSON.stringify(directories.value.sort())

  if (hasChanged) {
    emit('directoriesChanged')
  }

  emit('progressStep')
}

onMounted(async () => {
  const directoriesFromDB = await invoke('get_directories')
  if (directoriesFromDB && directoriesFromDB.length > 0) {
    directories.value = [...directoriesFromDB]
    originalDirectories.value = [...directoriesFromDB]
  } else {
    const dirPath = await audioDir()
    directories.value.push(dirPath)
    originalDirectories.value = [dirPath]
  }
  await loadParkedFolders()
})

const chooseDirectory = async () => {
  const selected = await open({
    directory: true,
    recursive: true,
  })

  if (selected && !directories.value.includes(selected)) {
    directories.value.push(selected)
  }
}

const removeDirectory = index => {
  if (index < 0) {
    return
  }
  directories.value.splice(index, 1)
}
</script>
