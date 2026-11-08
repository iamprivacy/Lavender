<template>
  <Transition name="fade">
    <div
      v-if="visible"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/70"
      @click.self="answer(false)"
    >
      <div class="modal-content p-6 gap-4 flex flex-col w-full max-w-md" role="dialog">
        <div class="text-lg text-neutral-800 dark:text-neutral-300">
          Want to hear about updates?
        </div>
        <div class="text-sm text-neutral-600 dark:text-neutral-400">
          Lavender can look for a new version of itself on its GitHub releases page when it starts,
          once a day. Nothing is downloaded or installed unless you choose to, and you can change
          this any time in About.
        </div>
        <div class="flex justify-end gap-2">
          <button class="button button-normal px-4 h-8 rounded-full text-xs" @click="answer(false)">
            Not now
          </button>
          <button class="button button-primary px-4 h-8 rounded-full text-xs" @click="answer(true)">
            Turn on update checks
          </button>
        </div>
      </div>
    </div>
  </Transition>
</template>

<script setup>
import { ref } from 'vue'
import { useAppUpdate } from '@/composables/app-update.js'

defineProps({
  visible: {
    type: Boolean,
    default: false,
  },
})

const emit = defineEmits(['answered'])
const { resolveOptIn } = useAppUpdate()
const answering = ref(false)

// Closing the prompt by clicking outside counts as "Not now".
const answer = async accept => {
  if (answering.value) {
    return
  }
  answering.value = true
  emit('answered')
  try {
    await resolveOptIn(accept)
  } finally {
    answering.value = false
  }
}
</script>
