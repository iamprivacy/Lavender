<template>
  <!-- Lavender's About dialog. Upstream's About.vue stays untouched (it checks
       LRCGET's releases and lists LRCGET's contacts); Library.vue opens this one. -->
  <!-- Sized to its content, up to the default 80vh, so no blank space sits below the credit. -->
  <BaseModal
    title="About"
    content-class="w-full max-h-[80vh] max-w-screen-sm"
    @close="emit('close')"
  >
    <div class="overflow-auto">
      <AppUpdateSection class="mb-6" />

      <SupportSection class="mb-6" />

      <div>
        <label class="group-label mb-2">Home page, contact and support</label>

        <div class="text-sm flex flex-col gap-1">
          <div>
            GitHub:
            <a href="https://github.com/iamprivacy/Lavender" class="link" target="_blank"
              >github.com/iamprivacy/Lavender</a
            >
          </div>

          <div>
            Questions and bug reports:
            <a href="https://github.com/iamprivacy/Lavender/issues" class="link" target="_blank"
              >github.com/iamprivacy/Lavender/issues</a
            >
          </div>

          <div class="text-neutral-600 dark:text-neutral-400">
            Forked from
            <a href="https://github.com/tranxuanthang/lrcget" class="link" target="_blank"
              >LRCGET</a
            >, the original app by tranxuanthang.
          </div>
        </div>
      </div>
    </div>
  </BaseModal>
</template>

<script setup>
import BaseModal from '@/components/common/BaseModal.vue'
import AppUpdateSection from '@/components/about/AppUpdateSection.vue'
import SupportSection from '@/components/about/SupportSection.vue'

const emit = defineEmits(['close'])
</script>
