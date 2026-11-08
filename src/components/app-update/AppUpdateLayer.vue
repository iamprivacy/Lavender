<template>
  <!-- Lavender: the update toast and the one-time opt-in prompt, mounted once
       by Library.vue. The launch check runs here and only when the user has
       turned automatic checks on. -->
  <UpdateToast />
  <UpdateOptInPrompt :visible="showPrompt" @answered="promptAnswered = true" />
</template>

<script setup>
import { computed, onMounted, ref } from 'vue'
import UpdateToast from '@/components/app-update/UpdateToast.vue'
import UpdateOptInPrompt from '@/components/app-update/UpdateOptInPrompt.vue'
import { useAppUpdate } from '@/composables/app-update.js'

const props = defineProps({
  // The library is on screen (not loading or scanning).
  ready: {
    type: Boolean,
    default: false,
  },
})

const PROMPT_DELAY_MS = 1500

const { info, loadInfo, startupCheck } = useAppUpdate()
const promptAnswered = ref(false)
const promptDue = ref(false)

const showPrompt = computed(
  () => props.ready && promptDue.value && !promptAnswered.value && !!info.value?.shouldPrompt
)

onMounted(async () => {
  try {
    await loadInfo()
  } catch (error) {
    console.error(error)
    return
  }
  startupCheck()
  setTimeout(() => {
    promptDue.value = true
  }, PROMPT_DELAY_MS)
})
</script>
