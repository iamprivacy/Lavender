import { beforeEach, describe, expect, it, vi } from 'vitest'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }))

beforeEach(() => {
  vi.resetModules()
  vi.resetAllMocks()
})

let revision = 0
const run = (overrides = {}) => ({
  revision: ++revision,
  exists: true,
  status: 'running',
  total: 3,
  done: 1,
  success: 1,
  failure: 0,
  kept: 0,
  skipped: 2,
  pauseReason: null,
  waiting: null,
  ...overrides,
})

const entry = (id, status = 'success') => ({
  id,
  status,
  title: `Song ${id}`,
  artistName: 'Artist',
  message: 'Synced lyrics downloaded',
})

// Attaches with a backend that answers the snapshot and hands back the
// event handler, so a test can play events.
async function attach(snapshot) {
  let handler
  listen.mockImplementation(async (name, callback) => {
    expect(name).toBe('download-run')
    handler = callback
    return () => {}
  })
  invoke.mockImplementation(async command => (command === 'get_download_run' ? snapshot : null))
  const { useDownloader } = await import('./downloader.js')
  const downloader = useDownloader()
  await downloader.downloadNext()
  return { downloader, emit: payload => handler({ payload }) }
}

describe('useDownloader', () => {
  it('mirrors the snapshot and the events of the backend run', async () => {
    const { downloader, emit } = await attach({ run: run(), log: [entry(1)] })
    expect(downloader.isDownloading.value).toBe(true)
    expect(downloader.isActive.value).toBe(true)
    expect(downloader.totalCount.value).toBe(3)
    expect(downloader.downloadedCount.value).toBe(1)
    expect(downloader.skippedCount.value).toBe(2)
    expect(downloader.downloadProgress.value).toBeCloseTo(1 / 3)
    expect(downloader.log.value.map(item => item.id)).toEqual([1])
    expect(downloader.needsAttention.value).toBe(false)

    emit({ run: run({ done: 2, kept: 1 }), entry: entry(2, 'kept') })
    expect(downloader.keptCount.value).toBe(1)
    expect(downloader.log.value.map(item => item.id)).toEqual([2, 1])
    emit({ run: run({ done: 2, kept: 1 }), entry: { ...entry(1), status: 'failure' } })
    expect(downloader.log.value.map(item => item.id)).toEqual(
      [1, 2],
      'a retried track replaces its line'
    )
    expect(downloader.log.value[0].status).toBe('failure')

    emit({ run: run({ done: 3, status: 'finished' }), entry: entry(3) })
    expect(downloader.isFinished.value).toBe(true)
    expect(downloader.isActive.value).toBe(false)
    expect(downloader.downloadProgress.value).toBe(1)

    emit({ run: run({ exists: false, status: 'none', total: 0, done: 0 }), entry: null })
    expect(downloader.isDownloading.value).toBe(false)
    expect(downloader.log.value).toEqual([])
    expect(listen).toHaveBeenCalledTimes(1)
    await downloader.downloadNext()
    expect(listen).toHaveBeenCalledTimes(1, 'attached once')
  })

  it('ignores a snapshot, an event or an answer older than what it holds', async () => {
    const { downloader, emit } = await attach({ run: run({ done: 0 }), log: [] })
    const newest = run({ done: 2 })
    emit({ run: newest, entry: entry(2) })
    emit({ run: { ...newest, revision: newest.revision - 1, done: 1 }, entry: entry(1) })
    expect(downloader.downloadedCount.value).toBe(2)
    expect(downloader.log.value.map(item => item.id)).toEqual([2], 'the stale entry is not added')
    invoke.mockResolvedValue({
      run: { ...newest, revision: newest.revision - 2, done: 0 },
      log: [],
    })
    await downloader.refresh()
    expect(downloader.downloadedCount.value).toBe(2)
    expect(downloader.log.value).toHaveLength(1)
    invoke.mockResolvedValue({ ...newest, revision: newest.revision - 1, status: 'paused' })
    await downloader.pauseDownloading()
    expect(downloader.isPaused.value).toBe(false)
    emit({ run: { ...newest, status: 'paused' }, entry: null })
    expect(downloader.isPaused.value).toBe(false, 'an event of the held revision says nothing new')
    // A stale event of a run that was cleared cannot bring it back: the
    // clearing answers with a revision above every event of the old run.
    invoke.mockResolvedValue(run({ exists: false, status: 'none' }))
    await downloader.stopDownloading()
    emit({ run: newest, entry: entry(2) })
    expect(downloader.isDownloading.value).toBe(false)
    expect(downloader.log.value).toEqual([])
    emit({ run: run({ total: 1, done: 0 }), entry: null })
    expect(downloader.isDownloading.value).toBe(true, 'a newer run is taken')
  })

  it('keeps at most a hundred log lines, newest first', async () => {
    const { downloader, emit } = await attach({ run: run(), log: [] })
    for (let id = 1; id <= 105; id++) emit({ run: run(), entry: entry(id) })
    expect(downloader.log.value).toHaveLength(100)
    expect(downloader.log.value[0].id).toBe(105)
    expect(downloader.log.value[99].id).toBe(6)
  })

  it('asks for attention when the backend reports an interrupted run', async () => {
    const { downloader } = await attach({
      run: run({ status: 'paused', pauseReason: 'interrupted' }),
      log: [],
    })
    expect(downloader.isPaused.value).toBe(true)
    expect(downloader.isInterrupted.value).toBe(true)
    expect(downloader.isActive.value).toBe(false)
    expect(downloader.needsAttention.value).toBe(true)
  })

  it('queues tracks through the backend and sends the options once', async () => {
    const { downloader } = await attach({ run: run({ exists: false }), log: [] })
    invoke.mockResolvedValue({ run: run({ total: 2, done: 0 }), log: [] })
    const autoExport = { plainText: true, syncedLrc: true, embedIntoTrack: false }
    await downloader.addToQueue([7, 8], autoExport, false, 0)
    expect(invoke).toHaveBeenLastCalledWith('start_download_run', {
      trackIds: [7, 8],
      autoExport,
    })
    expect(downloader.totalCount.value).toBe(2)
    await downloader.addToQueue([9], null, true, 5)
    expect(invoke).toHaveBeenLastCalledWith('start_download_run', {
      trackIds: [9],
      autoExport: null,
      upgrade: true,
      skipped: 5,
    })
  })

  it('pauses, resumes, retries and clears through the backend', async () => {
    const { downloader } = await attach({ run: run(), log: [entry(1)] })
    invoke.mockResolvedValue(run({ status: 'paused', pauseReason: 'user' }))
    await downloader.pauseDownloading()
    expect(invoke).toHaveBeenLastCalledWith('pause_download_run')
    expect(downloader.isPaused.value).toBe(true)
    invoke.mockResolvedValue(run())
    await downloader.resumeDownloading()
    expect(invoke).toHaveBeenLastCalledWith('resume_download_run')
    expect(downloader.isActive.value).toBe(true)
    invoke.mockImplementation(async command =>
      command === 'get_download_run' ? { run: run({ failure: 0 }), log: [] } : run({ failure: 0 })
    )
    await downloader.retryFailed()
    expect(invoke.mock.calls.slice(-2).map(([command]) => command)).toEqual([
      'retry_failed_downloads',
      'get_download_run',
    ])
    expect(downloader.log.value).toEqual([], 'the requeued tracks left the log')
    invoke.mockResolvedValue(undefined)
    await downloader.stopDownloading()
    expect(invoke).toHaveBeenLastCalledWith('clear_download_run')
    expect(downloader.isDownloading.value).toBe(false)
    expect(downloader.log.value).toEqual([])
  })

  it('reads the run again when an action fails', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const { downloader } = await attach({ run: run(), log: [] })
    invoke.mockImplementation(async command => {
      if (command === 'pause_download_run') throw new Error('database is locked')
      return { run: run({ done: 2 }), log: [] }
    })
    await downloader.pauseDownloading()
    expect(downloader.downloadedCount.value).toBe(2)
    expect(warn).toHaveBeenCalled()
  })
})
