<template>
  <div>
    <label class="group-label mb-4">Lyrics source</label>

    <div v-if="saveError" class="text-xs text-red-700 dark:text-red-400 mb-4">
      These settings were not saved: {{ saveError }}
    </div>

    <div class="flex flex-col mb-4">
      <div class="flex gap-4">
        <RadioButton
          id="lyrics-source-online"
          v-model="lyricsSource"
          name="lyrics-source"
          value="online"
        >
          Online LRCLIB
        </RadioButton>

        <RadioButton
          id="lyrics-source-local"
          v-model="lyricsSource"
          name="lyrics-source"
          value="local"
        >
          Local database
        </RadioButton>
      </div>
    </div>

    <div v-if="lyricsSource === 'local'" class="flex flex-col gap-4">
      <div class="flex flex-col">
        <label class="block mb-2 child-label" for="local-db-path">LRCLIB database file</label>
        <div class="flex gap-2 items-center">
          <input
            id="local-db-path"
            v-model="localDbPath"
            type="text"
            placeholder="lrclib-db-dump-....sqlite3"
            class="input px-4 h-8 flex-1"
            @change="validate"
          />
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :disabled="running"
            @click="browse"
          >
            Browse
          </button>
        </div>
        <span class="text-xs text-neutral-500 dark:text-neutral-400 mt-1"
          >A .sqlite3 or .db dump, or a .gz archive to decompress here. Get dumps at
          lrclib.net/db-dumps.</span
        >

        <div v-if="validating" class="text-xs text-neutral-500 dark:text-neutral-400 mt-2">
          Checking the database...
        </div>
        <div
          v-else-if="info"
          class="mt-2 text-xs rounded border border-neutral-200 dark:border-neutral-700 p-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-neutral-700 dark:text-neutral-300"
        >
          <span class="text-neutral-500 dark:text-neutral-400">File</span>
          <span class="break-all">{{ info.path }}</span>
          <span class="text-neutral-500 dark:text-neutral-400">Size</span>
          <span>{{ humanSize(info.fileSize) }}</span>
          <span class="text-neutral-500 dark:text-neutral-400">Tracks</span>
          <span>{{ countText(info.trackCount) }}</span>
          <span class="text-neutral-500 dark:text-neutral-400">Lyrics</span>
          <span>{{ countText(info.lyricsCount) }}</span>
          <span class="text-neutral-500 dark:text-neutral-400">Dump date</span>
          <span>{{ info.dumpDate || 'unknown' }}</span>
          <span class="text-neutral-500 dark:text-neutral-400">Search index</span>
          <span>{{
            info.hasFts ? 'available' : 'missing (keyword search needs online LRCLIB)'
          }}</span>
          <template v-if="info.missingIndexes.length">
            <span class="text-neutral-500 dark:text-neutral-400">Missing indexes</span>
            <span class="text-yellow-700 dark:text-yellow-400">{{
              info.missingIndexes.join(', ')
            }}</span>
          </template>
        </div>
        <div v-else-if="validationError" class="text-xs text-red-700 dark:text-red-400 mt-2">
          {{ validationError }}
        </div>
      </div>

      <div
        v-if="gzSource"
        class="flex flex-col gap-3 rounded border border-neutral-200 dark:border-neutral-700 p-3"
      >
        <span class="child-label"
          >This is a .gz archive. Decompress it into a folder with enough free space and the result
          becomes the database file.</span
        >
        <span class="text-xs break-all text-neutral-700 dark:text-neutral-300">{{ gzSource }}</span>

        <div class="flex gap-2 items-center">
          <input
            v-model="destDir"
            type="text"
            readonly
            placeholder="Destination folder"
            class="input px-4 h-8 flex-1"
          />
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :disabled="running"
            @click="chooseDestDir"
          >
            Choose folder
          </button>
        </div>

        <div v-if="spaceCheck" class="text-xs text-neutral-700 dark:text-neutral-300">
          Needs about {{ humanSize(spaceCheck.estimatedBytes) }} (an estimate);
          {{ humanSize(spaceCheck.availableBytes) }} free in this folder.
          <span v-if="!spaceCheck.enough" class="text-red-700 dark:text-red-400"
            >That may not be enough.</span
          >
        </div>

        <CheckboxButton id="local-db-delete-gz" v-model="deleteSource" name="local-db-delete-gz">
          Delete the .gz archive after decompressing
        </CheckboxButton>
        <CheckboxButton
          v-if="spaceCheck && !spaceCheck.enough"
          id="local-db-skip-space"
          v-model="skipSpaceCheck"
          name="local-db-skip-space"
        >
          Start anyway
        </CheckboxButton>

        <div v-if="progress" class="flex flex-col gap-1">
          <div class="h-2 rounded-full bg-neutral-200 dark:bg-neutral-700 overflow-hidden">
            <div
              class="h-full bg-hoa-1100 transition-all"
              :style="{ width: `${Math.max(0, Math.min(100, progress.percent))}%` }"
            ></div>
          </div>
          <span class="text-xs text-neutral-500 dark:text-neutral-400">
            {{ progress.percent.toFixed(1) }}% of the archive read,
            {{ humanSize(progress.bytesWritten) }} written
            <template v-if="progress.done"> (done)</template>
            <template v-else-if="progress.cancelled"> (cancelled)</template>
          </span>
        </div>
        <div
          v-if="progress && progress.note"
          class="text-xs text-neutral-500 dark:text-neutral-400"
        >
          {{ progress.note }}
        </div>
        <div v-if="decompressError" class="text-xs text-red-700 dark:text-red-400">
          {{ decompressError }}
        </div>

        <div class="flex gap-2">
          <button
            class="button px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :class="!destDir || running ? 'button-disabled' : 'button-primary'"
            :disabled="!destDir || running"
            @click="startDecompress"
          >
            Decompress
          </button>
          <button
            v-if="running"
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            @click="cancelDecompress"
          >
            Cancel
          </button>
          <button
            v-else
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            @click="dismissGz"
          >
            Dismiss
          </button>
        </div>
      </div>

      <div
        class="flex flex-col gap-3 rounded border border-neutral-200 dark:border-neutral-700 p-3"
      >
        <div class="flex items-center justify-between gap-2">
          <span class="child-label">{{
            info ? 'Update the database' : 'Download the database'
          }}</span>
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :disabled="checkingDump || updateRunning"
            @click="checkDump"
          >
            Check for updates
          </button>
        </div>
        <div v-if="dumpCheck" class="text-xs text-neutral-700 dark:text-neutral-300">
          Latest dump: {{ dumpCheck.latest.date || dumpCheck.latest.key }},
          {{ humanSize(dumpCheck.latest.size) }}.
          <template v-if="dumpCheck.currentDate">
            Yours: {{ dumpCheck.currentDate }}
            <span :class="dumpCheck.newer ? 'text-yellow-700 dark:text-yellow-400' : ''"
              >({{ dumpCheck.newer ? 'update available' : 'up to date' }})</span
            >
          </template>
          <template v-else>No database is installed yet.</template>
        </div>
        <div v-if="dumpCheckError" class="text-xs text-red-700 dark:text-red-400">
          {{ dumpCheckError }}
        </div>

        <div class="flex flex-col">
          <label
            class="block mb-1 text-xs text-neutral-500 dark:text-neutral-400"
            for="local-db-update-url"
            >Dump URL</label
          >
          <input
            id="local-db-update-url"
            v-model="updateUrl"
            type="text"
            placeholder="https://db-dumps.lrclib.net/lrclib-db-dump-....sqlite3.gz"
            class="input px-4 h-8"
            :disabled="updateRunning"
          />
        </div>
        <label class="block -mb-2 text-xs text-neutral-500 dark:text-neutral-400"
          >Download folder</label
        >
        <div class="flex gap-2 items-center">
          <input
            v-model="downloadDir"
            type="text"
            readonly
            placeholder="Where the archive is saved"
            class="input px-4 h-8 flex-1"
          />
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :disabled="updateRunning"
            @click="chooseUpdateDir('download')"
          >
            Choose folder
          </button>
        </div>
        <label class="block -mb-2 text-xs text-neutral-500 dark:text-neutral-400"
          >Database folder</label
        >
        <div class="flex gap-2 items-center">
          <input
            v-model="databaseDir"
            type="text"
            readonly
            placeholder="Where the new database goes"
            class="input px-4 h-8 flex-1"
          />
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :disabled="updateRunning"
            @click="chooseUpdateDir('database')"
          >
            Choose folder
          </button>
        </div>
        <div class="flex gap-2 items-center text-xs text-neutral-700 dark:text-neutral-300">
          <label for="local-db-update-connections">Connections</label>
          <select
            id="local-db-update-connections"
            v-model.number="connections"
            class="input px-2 h-8"
            :disabled="updateRunning"
          >
            <option v-for="n in [1, 2, 4, 6, 8]" :key="n" :value="n">{{ n }}</option>
          </select>
          <span class="text-neutral-500 dark:text-neutral-400"
            >More connections download faster.</span
          >
        </div>

        <div
          class="text-xs"
          :class="
            updateSpace?.enough
              ? 'text-neutral-500 dark:text-neutral-400'
              : 'text-red-700 dark:text-red-400'
          "
        >
          Needs about {{ archiveSize ? humanSize(archiveSize) : '48 GB' }} for the archive and
          {{ archiveSize ? humanSize(archiveSize * 2.5) : '120 GB' }} for the new database<template
            v-if="info"
            >, and the current one ({{ humanSize(info.fileSize) }}) stays until the update is
            done</template
          >.
          <template v-if="updateSpace">
            Free now: {{ humanSize(updateSpace.downloadFree) }} in the download folder<template
              v-if="!updateSpace.sameFolder"
              >, {{ humanSize(updateSpace.databaseFree) }} in the database folder</template
            >.
            <strong v-if="!updateSpace.enough">That is not enough.</strong>
          </template>
        </div>
        <div v-if="updateSpaceError" class="text-xs text-red-700 dark:text-red-400">
          {{ updateSpaceError }}
        </div>

        <CheckboxButton
          id="local-db-update-delete-archive"
          v-model="deleteArchive"
          name="local-db-update-delete-archive"
        >
          Delete the archive after decompressing
        </CheckboxButton>
        <CheckboxButton
          v-if="info"
          id="local-db-update-delete-previous"
          v-model="deletePrevious"
          name="local-db-update-delete-previous"
        >
          Delete the old database after updating
        </CheckboxButton>
        <CheckboxButton
          v-if="updateSpace && !updateSpace.enough"
          id="local-db-update-skip-space"
          v-model="updateSkipSpace"
          name="local-db-update-skip-space"
        >
          Start anyway
        </CheckboxButton>

        <div
          v-if="updateProgress && updateProgress.stage !== 'idle'"
          class="flex flex-col gap-1 text-xs"
        >
          <span class="text-neutral-700 dark:text-neutral-300">{{ stageText }}</span>
          <template v-if="updateProgress.download">
            <div class="h-2 rounded-full bg-neutral-200 dark:bg-neutral-700 overflow-hidden">
              <div
                class="h-full bg-hoa-1100 transition-all"
                :style="{
                  width: `${Math.max(0, Math.min(100, updateProgress.download.percent))}%`,
                }"
              ></div>
            </div>
            <span class="text-neutral-500 dark:text-neutral-400">
              {{ updateProgress.download.percent.toFixed(1) }}% downloaded,
              {{ humanSize(updateProgress.download.bytesDone)
              }}<template v-if="updateProgress.download.totalBytes">
                of {{ humanSize(updateProgress.download.totalBytes) }}</template
              ><template v-if="updateProgress.stage === 'downloading'"
                >, {{ humanSpeed(updateProgress.download.bytesPerSecond) || 'connecting' }},
                {{ updateProgress.download.activeConnections }} connections</template
              >
            </span>
          </template>
          <template v-if="updateProgress.decompress">
            <div class="h-2 rounded-full bg-neutral-200 dark:bg-neutral-700 overflow-hidden">
              <div
                class="h-full bg-hoa-1100 transition-all"
                :style="{
                  width: `${Math.max(0, Math.min(100, updateProgress.decompress.percent))}%`,
                }"
              ></div>
            </div>
            <span class="text-neutral-500 dark:text-neutral-400">
              {{ updateProgress.decompress.percent.toFixed(1) }}% of the archive read,
              {{ humanSize(updateProgress.decompress.bytesWritten) }} written
            </span>
          </template>
          <div v-if="updateProgress.error" class="text-red-700 dark:text-red-400">
            {{ updateProgress.error }}
          </div>
          <div v-else-if="updateProgress.message" class="text-neutral-500 dark:text-neutral-400">
            {{ updateProgress.message }}
          </div>
        </div>
        <div v-if="updateError" class="text-xs text-red-700 dark:text-red-400">
          {{ updateError }}
        </div>
        <div
          v-if="resumableBytes && !resumableFolderChosen && !updateRunning"
          class="text-xs text-yellow-700 dark:text-yellow-400"
        >
          A stopped download ({{ humanSize(resumableBytes) }}) is in {{ resumableFolder }}. Pick
          that download folder to resume it; any other folder starts over.
        </div>
        <div
          v-else-if="resumableBytes && !resumesHere && !updateRunning"
          class="text-xs text-yellow-700 dark:text-yellow-400"
        >
          The stopped download ({{ humanSize(resumableBytes) }}) in this folder is from another
          link. This one starts over; discard the old file first to free its space.
        </div>

        <div class="flex gap-2">
          <button
            class="button px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :class="canStartUpdate ? 'button-primary' : 'button-disabled'"
            :disabled="!canStartUpdate"
            @click="startUpdate"
          >
            {{ startLabel }}
          </button>
          <button
            v-if="updateRunning"
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            @click="pauseUpdate"
          >
            Stop
          </button>
          <button
            v-else-if="resumableBytes"
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            @click="discardUpdate"
          >
            Discard download
          </button>
        </div>
      </div>

      <CheckboxButton
        id="local-db-fallback-online"
        v-model="fallbackOnline"
        name="local-db-fallback-online"
      >
        If not found locally, try online LRCLIB
      </CheckboxButton>

      <CheckboxButton
        id="local-db-tolerant-matching"
        v-model="tolerantMatching"
        name="local-db-tolerant-matching"
      >
        If there is no exact match, match on title, artists and length (local only)
      </CheckboxButton>

      <CheckboxButton
        id="local-db-instrumental-titles"
        v-model="instrumentalTitles"
        name="local-db-instrumental-titles"
      >
        Mark tracks titled "... (Instrumental)" as instrumental (local only)
      </CheckboxButton>

      <CheckboxButton id="local-db-skip-checked" v-model="skipChecked" name="local-db-skip-checked">
        Skip tracks already checked. A track is checked again after a retag, a lyrics edit or a new
        dump; online lookups are retried after 14 days.
      </CheckboxButton>
      <div
        v-if="checkedCount > 0 || upgradeCheckedCount > 0"
        class="flex gap-3 items-center text-xs text-neutral-500 dark:text-neutral-400"
      >
        <span>
          Remembered: {{ checkedCount.toLocaleString() }} not found,
          {{ upgradeCheckedCount.toLocaleString() }} kept by Upgrade.
        </span>
        <button
          class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
          @click="forgetChecked"
        >
          Clear
        </button>
      </div>
    </div>

    <div class="flex flex-col gap-2 mt-6">
      <div class="text-sm font-bold">Extra sources</div>
      <div class="text-xs text-neutral-500 dark:text-neutral-400">
        Used only when LRCLIB has nothing for a track, best source first. The download log names the
        source. Publishing and flagging always go to LRCLIB.
      </div>
      <div class="text-xs text-neutral-500 dark:text-neutral-400">
        Each source you turn on receives the title and artist of tracks LRCLIB could not find
        (music.163.com for NetEase, c.y.qq.com for QQ Music; AMLL searches through those two and
        downloads from raw.githubusercontent.com). All are off by default.
      </div>
      <CheckboxButton id="extra-amll" v-model="extraAmll" name="extra-amll">
        AMLL TTML database: word-synced lyrics from a public collection on GitHub, mostly Chinese,
        Japanese and Korean pop
      </CheckboxButton>
      <CheckboxButton id="extra-apple" v-model="extraApple" name="extra-apple">
        Apple Music: word-synced lyrics, needs your subscription
      </CheckboxButton>
      <div v-if="extraApple" class="flex flex-col gap-2 ml-6">
        <div class="text-xs text-neutral-500 dark:text-neutral-400">
          Sign in opens music.apple.com in a private window. Your password goes to Apple only;
          Lavender keeps just the media-user-token cookie and closes the window. You can also paste
          that cookie from a browser's developer tools. This uses an undocumented API, against Apple
          Music's terms of use. The token is stored unencrypted in Lavender's settings and sent only
          to amp-api.music.apple.com, with each track's title and artist.
        </div>
        <div class="flex gap-2 items-center">
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :disabled="appleSigningIn || appleChecking"
            @click="signInToApple"
          >
            {{ appleSigningIn ? 'Waiting for sign-in...' : 'Sign in' }}
          </button>
          <input
            id="apple-user-token"
            v-model="appleUserToken"
            type="password"
            autocomplete="off"
            placeholder="or paste the media-user-token"
            class="input px-4 h-8 flex-1"
            @input="appleCheck = null"
          />
          <button
            class="button button-normal px-4 h-8 rounded-full text-xs whitespace-nowrap shrink-0"
            :disabled="appleChecking || appleSigningIn || !appleUserToken.trim()"
            @click="checkAppleToken"
          >
            {{ appleChecking ? 'Checking...' : 'Check' }}
          </button>
        </div>
        <div
          v-if="appleCheck"
          class="text-xs"
          :class="
            appleCheck.ok ? 'text-green-700 dark:text-green-400' : 'text-red-700 dark:text-red-400'
          "
        >
          {{ appleCheck.text }}
        </div>
      </div>
      <CheckboxButton id="extra-netease" v-model="extraNetease" name="extra-netease">
        NetEase Cloud Music: synced lyrics, some word-synced (unofficial API)
      </CheckboxButton>
      <CheckboxButton id="extra-qq" v-model="extraQq" name="extra-qq">
        QQ Music: synced lyrics (unofficial API)
      </CheckboxButton>
      <CheckboxButton id="extra-plain-lyrics" v-model="extraPlainLyrics" name="extra-plain-lyrics">
        Also accept unsynced lyrics from extra sources
      </CheckboxButton>
    </div>

    <div class="flex flex-col gap-2 mt-6">
      <div class="text-sm font-bold">Lyrics edition</div>
      <div class="text-xs text-neutral-500 dark:text-neutral-400">
        Many songs have an explicit version and a clean one with swearing masked ("b***h"). This
        picks which the local database and extra sources use when both exist; online LRCLIB results
        are used as they come. The download log says "masked words" when only a clean version was
        found, and "Upgrade to the best match" replaces those once uncensored lyrics turn up.
      </div>
      <div class="flex flex-col gap-2">
        <RadioButton
          id="lyrics-edition-follow-file"
          v-model="lyricsEdition"
          name="lyrics-edition"
          value="follow-file"
        >
          Uncensored, unless the file's title or album says Clean
        </RadioButton>
        <RadioButton
          id="lyrics-edition-explicit"
          v-model="lyricsEdition"
          name="lyrics-edition"
          value="explicit"
        >
          Always uncensored
        </RadioButton>
        <RadioButton
          id="lyrics-edition-clean"
          v-model="lyricsEdition"
          name="lyrics-edition"
          value="clean"
        >
          Always clean
        </RadioButton>
      </div>
    </div>

    <div class="flex flex-col gap-2 mt-6">
      <div class="text-sm font-bold">Export</div>
      <CheckboxButton
        id="export-word-timestamps"
        v-model="exportWordTimestamps"
        name="export-word-timestamps"
      >
        Add word timestamps to exported .lrc files when available (enhanced LRC, for players like
        Lyricify, Walaoke and AMLL; others may show or ignore them)
      </CheckboxButton>
    </div>
  </div>
</template>

<script setup>
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useToast } from 'vue-toastification'
import RadioButton from '@/components/common/RadioButton.vue'
import CheckboxButton from '@/components/common/CheckboxButton.vue'
import {
  humanSize,
  humanSpeed,
  parentFolder,
  useLyricsSource,
} from '@/composables/lyrics-source.js'

const { loadLyricsSource, saveForkConfig, saveError } = useLyricsSource()
const toast = useToast()

const lyricsSource = ref('online')
const localDbPath = ref('')
const fallbackOnline = ref(true)
const tolerantMatching = ref(true)
const skipChecked = ref(true)
const instrumentalTitles = ref(true)
const extraAmll = ref(false)
const extraApple = ref(false)
const appleUserToken = ref('')
// The result of the Check button: { ok, text }, cleared when the token changes.
const appleCheck = ref(null)
const appleChecking = ref(false)
const appleSigningIn = ref(false)
const extraNetease = ref(false)
const extraQq = ref(false)
const extraPlainLyrics = ref(false)
const exportWordTimestamps = ref(false)
const lyricsEdition = ref('follow-file')
const checkedCount = ref(0)
const upgradeCheckedCount = ref(0)

const info = ref(null)
const validationError = ref(null)
const validating = ref(false)

const gzSource = ref(null)
const destDir = ref('')
const spaceCheck = ref(null)
const deleteSource = ref(false)
const skipSpaceCheck = ref(false)
const running = ref(false)
const progress = ref(null)
const decompressError = ref(null)

let unlistenProgress = null
let unlistenUpdate = null

// Update the database (download, decompress, install) from inside the app.
const ASSUMED_ARCHIVE_BYTES = 48_000_000_000
const checkingDump = ref(false)
const dumpCheck = ref(null)
const dumpCheckError = ref(null)
const updateUrl = ref('')
const downloadDir = ref('')
const databaseDir = ref('')
const connections = ref(4)
const deleteArchive = ref(true)
const deletePrevious = ref(false)
const updateSkipSpace = ref(false)
const updateSpace = ref(null)
const updateSpaceError = ref(null)
const updateProgress = ref(null)
const updateRunningFlag = ref(false)
const resumableBytes = ref(null)
// The stopped download's archive on disk; it can only continue in its folder.
const resumableArchive = ref(null)
// The link it was downloading; another link is another download.
const resumableUrl = ref(null)
const updateError = ref(null)

const archiveSize = computed(() =>
  dumpCheck.value && updateUrl.value.trim() === dumpCheck.value.latest.url
    ? dumpCheck.value.latest.size
    : null
)
const updateRunning = computed(
  () =>
    updateRunningFlag.value ||
    ['downloading', 'decompressing', 'installing'].includes(updateProgress.value?.stage)
)
const canStartUpdate = computed(
  () =>
    !updateRunning.value &&
    updateUrl.value.trim().length > 0 &&
    downloadDir.value.length > 0 &&
    databaseDir.value.length > 0
)
const resumableFolder = computed(() =>
  resumableBytes.value && resumableArchive.value ? parentFolder(resumableArchive.value) : ''
)
const resumableFolderChosen = computed(
  () => !!resumableFolder.value && resumableFolder.value === downloadDir.value
)
const resumesHere = computed(
  () =>
    resumableFolderChosen.value &&
    (!resumableUrl.value || resumableUrl.value === updateUrl.value.trim())
)
const startLabel = computed(() => {
  if (resumesHere.value) return 'Resume download'
  if (['stopped', 'failed'].includes(updateProgress.value?.stage)) return 'Start again'
  return info.value ? 'Update' : 'Download'
})
const stageText = computed(() => {
  switch (updateProgress.value?.stage) {
    case 'downloading':
      return 'Downloading the archive...'
    case 'decompressing':
      return 'Decompressing the archive...'
    case 'installing':
      return 'Checking the new database...'
    case 'done':
      return 'Done.'
    case 'paused':
      return 'Download stopped.'
    case 'stopped':
      return 'Stopped.'
    case 'failed':
      return 'The update failed.'
    default:
      return ''
  }
})

// Opening a database no longer counts its rows (a full index scan of a
// 100+ GB file); the counts are asked for once a database is shown here.
const countFailed = ref(false)
const countText = count =>
  count === null || count === undefined
    ? countFailed.value
      ? 'unknown'
      : 'counting...'
    : count.toLocaleString()

const fillCounts = async () => {
  const shown = info.value
  if (!shown || (shown.trackCount !== null && shown.trackCount !== undefined)) return
  countFailed.value = false
  try {
    const counted = await invoke('count_local_db_rows', { path: shown.path })
    if (info.value && info.value.path === counted.path) info.value = counted
  } catch {
    if (info.value === shown) countFailed.value = true
  }
}

watch(info, fillCounts)

// The path as the dialog opened, so Save can tell a path changed here.
let loadedPath = ''

const load = async () => {
  const { config, status } = await loadLyricsSource()
  lyricsSource.value = config.lyricsSource
  localDbPath.value = config.localDbPath || ''
  loadedPath = localDbPath.value.trim()
  fallbackOnline.value = config.fallbackOnline
  tolerantMatching.value = config.tolerantMatching !== false
  skipChecked.value = config.skipChecked !== false
  instrumentalTitles.value = config.instrumentalTitles !== false
  extraAmll.value = config.extraAmll === true
  extraApple.value = config.extraApple === true
  appleUserToken.value = config.appleUserToken || ''
  extraNetease.value = config.extraNetease === true
  extraQq.value = config.extraQq === true
  extraPlainLyrics.value = config.extraPlainLyrics === true
  exportWordTimestamps.value = config.exportWordTimestamps === true
  lyricsEdition.value = config.lyricsEdition || 'follow-file'
  info.value = status.open ? status.info : null
  await refreshCheckedCount()
  validationError.value = status.open ? null : status.error
  // In Online mode the remembered dump stays closed: opening it here would
  // switch the active database and count its rows for a source not in use.
  if (lyricsSource.value === 'local' && localDbPath.value && !status.open && !status.error) {
    await validate()
  }
}

// Switching to Local in the dialog shows the remembered database, if any.
watch(lyricsSource, async source => {
  if (source === 'local' && localDbPath.value && !info.value && !validationError.value) {
    await validate()
  }
})

// Throws when the backend refuses; the error stays shown at the top of this
// section and a toast says so now, since the settings dialog closes anyway.
const save = async () => {
  // A path typed or browsed to in this dialog that could not be opened would
  // replace the working database with nothing; the one loaded with the
  // dialog may be unreadable for now (an unplugged drive) and is kept.
  const path = localDbPath.value.trim()
  if (validationError.value && path && path !== loadedPath) {
    const error = `${path} could not be opened: ${validationError.value}`
    toast.error(`Lyrics source settings were not saved: ${error}`)
    throw new Error(error)
  }
  try {
    await saveForkConfig(currentConfig())
  } catch (error) {
    toast.error(`Lyrics source settings were not saved: ${error}`)
    throw error
  }
}

const currentConfig = () => ({
  lyricsSource: lyricsSource.value,
  localDbPath: localDbPath.value.trim() || null,
  fallbackOnline: fallbackOnline.value,
  tolerantMatching: tolerantMatching.value,
  skipChecked: skipChecked.value,
  instrumentalTitles: instrumentalTitles.value,
  extraAmll: extraAmll.value,
  extraApple: extraApple.value,
  appleUserToken: appleUserToken.value.trim() || null,
  extraNetease: extraNetease.value,
  extraQq: extraQq.value,
  extraPlainLyrics: extraPlainLyrics.value,
  exportWordTimestamps: exportWordTimestamps.value,
  lyricsEdition: lyricsEdition.value,
})

// Asks the backend to sign in with the token as typed (not yet saved) and
// shows the storefront it belongs to, or why Apple refused it.
const checkAppleToken = async () => {
  appleChecking.value = true
  appleCheck.value = null
  try {
    const result = await invoke('check_apple_token', { token: appleUserToken.value.trim() })
    appleCheck.value = { ok: true, text: `Signed in; storefront ${result.storefront}.` }
  } catch (error) {
    appleCheck.value = { ok: false, text: `${error}` }
  } finally {
    appleChecking.value = false
  }
}

// Opens the Apple Music sign-in window and, once the web player has a user
// token, puts it into the field (saved with the other settings) and checks it.
const signInToApple = async () => {
  appleSigningIn.value = true
  appleCheck.value = null
  try {
    const result = await invoke('apple_sign_in')
    appleUserToken.value = result.token
  } catch (error) {
    appleCheck.value = { ok: false, text: `${error}` }
    return
  } finally {
    appleSigningIn.value = false
  }
  await checkAppleToken()
}

const refreshCheckedCount = async () => {
  try {
    const checked = await invoke('get_checked_count')
    checkedCount.value = checked.count
    upgradeCheckedCount.value = checked.upgradeCount ?? 0
  } catch {
    checkedCount.value = 0
  }
}

const forgetChecked = async () => {
  await invoke('clear_checked_tracks')
  await refreshCheckedCount()
}

// A preview: the file is read and shown, but stays out of use until Save.
const validate = async () => {
  const path = localDbPath.value.trim()
  info.value = null
  validationError.value = null
  if (!path) return
  validating.value = true
  try {
    info.value = await invoke('validate_local_db', { path })
    await refreshCheckedCount()
  } catch (error) {
    validationError.value = String(error)
  } finally {
    validating.value = false
  }
}

// Not while a decompression runs: its events are matched to the chosen
// archive, so replacing it would leave the panel stuck on Cancel.
const browse = async () => {
  if (running.value) return
  const selected = await open({
    multiple: false,
    directory: false,
    filters: [
      { name: 'LRCLIB database', extensions: ['sqlite3', 'db', 'sqlite', 'gz'] },
      { name: 'All files', extensions: ['*'] },
    ],
  })
  if (!selected) return
  const path = typeof selected === 'string' ? selected : selected.path
  if (!path) return
  if (path.toLowerCase().endsWith('.gz')) {
    gzSource.value = path
    destDir.value = ''
    spaceCheck.value = null
    progress.value = null
    decompressError.value = null
  } else {
    localDbPath.value = path
    await validate()
  }
}

const chooseDestDir = async () => {
  const selected = await open({ directory: true, multiple: false })
  if (!selected) return
  destDir.value = typeof selected === 'string' ? selected : selected.path
  spaceCheck.value = null
  skipSpaceCheck.value = false
  decompressError.value = null
  try {
    spaceCheck.value = await invoke('check_local_db_space', {
      source: gzSource.value,
      destDir: destDir.value,
    })
  } catch (error) {
    decompressError.value = String(error)
  }
}

const startDecompress = async () => {
  decompressError.value = null
  progress.value = null
  running.value = true
  try {
    await invoke('start_local_db_decompress', {
      source: gzSource.value,
      destDir: destDir.value,
      deleteSource: deleteSource.value,
      skipSpaceCheck: skipSpaceCheck.value,
    })
  } catch (error) {
    running.value = false
    decompressError.value = String(error)
  }
}

const cancelDecompress = async () => {
  await invoke('cancel_local_db_decompress')
}

// A decompression the dialog was closed on: show it again, with Cancel while
// it runs and its error if it failed meanwhile. A run that finished while the
// dialog was closed already saved its result as the database path.
const refreshDecompressStatus = async () => {
  try {
    const status = await invoke('get_local_db_decompress_status')
    const last = status.progress
    if (!last || !(status.running || last.error)) return
    gzSource.value = last.source
    destDir.value = parentFolder(last.output)
    progress.value = last
    running.value = status.running
    decompressError.value = last.error || null
  } catch {
    // Nothing to restore.
  }
}

const dismissGz = () => {
  gzSource.value = null
  progress.value = null
  decompressError.value = null
}

const onProgress = async event => {
  const update = event.payload
  if (gzSource.value && update.source !== gzSource.value) return
  progress.value = update
  if (update.error) {
    running.value = false
    decompressError.value = update.error
  } else if (update.cancelled) {
    running.value = false
  } else if (update.done) {
    // The backend has made the result the database in use and saved its
    // path; the card shows it.
    running.value = false
    localDbPath.value = update.output
    gzSource.value = null
    await validate()
  }
}

const checkDump = async () => {
  checkingDump.value = true
  dumpCheckError.value = null
  try {
    dumpCheck.value = await invoke('check_dump_updates')
    if (!updateUrl.value.trim()) updateUrl.value = dumpCheck.value.latest.url
    await refreshUpdateSpace()
  } catch (error) {
    dumpCheckError.value = String(error)
  } finally {
    checkingDump.value = false
  }
}

const chooseUpdateDir = async which => {
  const selected = await open({ directory: true, multiple: false })
  if (!selected) return
  const path = typeof selected === 'string' ? selected : selected.path
  if (which === 'download') downloadDir.value = path
  else databaseDir.value = path
  await refreshUpdateSpace()
}

const refreshUpdateSpace = async () => {
  updateSpace.value = null
  updateSpaceError.value = null
  updateSkipSpace.value = false
  if (!downloadDir.value || !databaseDir.value) return
  try {
    updateSpace.value = await invoke('check_update_space', {
      downloadDir: downloadDir.value,
      databaseDir: databaseDir.value,
      archiveSize: archiveSize.value || ASSUMED_ARCHIVE_BYTES,
      // What a stopped download of this link already holds is not needed twice.
      url: updateUrl.value.trim() || null,
    })
  } catch (error) {
    updateSpaceError.value = String(error)
  }
}

const refreshUpdateStatus = async () => {
  try {
    const status = await invoke('get_local_db_update_status')
    updateRunningFlag.value = status.running
    updateProgress.value = status.progress
    resumableBytes.value = status.resumableBytes || null
    resumableArchive.value = status.resumableBytes ? status.progress?.archive || null : null
    resumableUrl.value = status.resumableBytes ? status.progress?.download?.url || null : null
    if (status.progress?.download?.url && !updateUrl.value) {
      updateUrl.value = status.progress.download.url
    }
    // The partial file continues only from its own folder, which may not be
    // the current database's (the default), so that folder is restored.
    if (resumableFolder.value && downloadDir.value !== resumableFolder.value) {
      downloadDir.value = resumableFolder.value
      await refreshUpdateSpace()
    }
  } catch {
    updateRunningFlag.value = false
  }
}

const startUpdate = async () => {
  updateError.value = null
  try {
    await invoke('start_local_db_update', {
      request: {
        url: updateUrl.value.trim(),
        size: archiveSize.value,
        downloadDir: downloadDir.value,
        databaseDir: databaseDir.value,
        connections: connections.value,
        deleteArchive: deleteArchive.value,
        deletePrevious: deletePrevious.value,
        skipSpaceCheck: updateSkipSpace.value,
      },
    })
    updateRunningFlag.value = true
    resumableBytes.value = null
    resumableArchive.value = null
    resumableUrl.value = null
  } catch (error) {
    updateError.value = String(error)
  }
}

const pauseUpdate = async () => {
  await invoke('pause_local_db_update')
}

const discardUpdate = async () => {
  updateError.value = null
  try {
    await invoke('discard_local_db_update')
    await refreshUpdateStatus()
  } catch (error) {
    updateError.value = String(error)
  }
}

const onUpdateProgress = async event => {
  updateProgress.value = event.payload
  const stage = event.payload?.stage
  if (['done', 'paused', 'stopped', 'failed'].includes(stage)) {
    updateRunningFlag.value = false
    await refreshUpdateStatus()
    if (stage === 'done') {
      await load()
      await refreshUpdateSpace()
    }
  }
}

// Both folders default to where the current database lives.
watch(info, current => {
  const folder = parentFolder(current?.path)
  if (!folder) return
  if (!downloadDir.value) downloadDir.value = folder
  if (!databaseDir.value) databaseDir.value = folder
  refreshUpdateSpace()
})

defineExpose({ load, save })

onMounted(async () => {
  unlistenProgress = await listen('local-db-decompress-progress', onProgress)
  unlistenUpdate = await listen('local-db-update-progress', onUpdateProgress)
  await load()
  await refreshDecompressStatus()
  await refreshUpdateStatus()
})

onUnmounted(() => {
  if (unlistenProgress) unlistenProgress()
  if (unlistenUpdate) unlistenUpdate()
})
</script>
