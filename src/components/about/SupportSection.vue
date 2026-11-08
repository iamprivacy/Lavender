<template>
  <div class="flex flex-col gap-6">
    <div>
      <label class="group-label mb-2">Support Lavender</label>

      <div class="text-sm flex flex-col gap-3">
        <div>
          If you enjoy Lavender, a star on the repo goes a long way, and if you'd like to help me
          keep developing it, consider donating.
        </div>
        <div>
          <!-- Drawn here instead of loading the badge image, so About makes no request. -->
          <a
            href="https://www.buymeacoffee.com/iamprivacy"
            target="_blank"
            class="inline-flex items-center gap-1.5 rounded px-3 h-8 text-xs font-bold text-black bg-[#ffdd00] hover:bg-[#ffe633] active:bg-[#f5d400] transition"
          >
            <Coffee class="text-base" />
            Buy me a coffee
          </a>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
import Coffee from '~icons/mdi/coffee'
</script>
