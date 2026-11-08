import { beforeEach, describe, expect, it, vi } from 'vitest'
import { invoke } from '@tauri-apps/api/core'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }))

beforeEach(() => {
  vi.resetModules()
  vi.resetAllMocks()
})

const localConfig = (skipChecked = true) => ({
  lyricsSource: 'local',
  localDbPath: '/x/dump.sqlite3',
  fallbackOnline: true,
  tolerantMatching: true,
  skipChecked,
})

describe('applySkipChecked', () => {
  it('asks the backend to drop the remembered misses in local mode', async () => {
    invoke.mockResolvedValue([2, 3])
    const { useLyricsSource } = await import('./lyrics-source.js')
    useLyricsSource().setForkConfig(localConfig())
    const { applySkipChecked } = await import('./skip-checked.js')
    expect(await applySkipChecked([1, 2, 3])).toEqual([2, 3])
    expect(invoke).toHaveBeenCalledWith('filter_checked_track_ids', { trackIds: [1, 2, 3] })
  })

  it('names the upgrade run and its export formats only for that choice', async () => {
    invoke.mockResolvedValue([3])
    const { useLyricsSource } = await import('./lyrics-source.js')
    useLyricsSource().setForkConfig(localConfig())
    const { applySkipChecked } = await import('./skip-checked.js')
    expect(await applySkipChecked([1, 3], { upgrade: true, exportFormats: ['lrc'] })).toEqual([3])
    expect(invoke).toHaveBeenLastCalledWith('filter_checked_track_ids', {
      trackIds: [1, 3],
      upgrade: true,
      exportFormats: ['lrc'],
    })
    await applySkipChecked([1, 3], { upgrade: false, exportFormats: ['lrc'] })
    expect(invoke).toHaveBeenLastCalledWith('filter_checked_track_ids', { trackIds: [1, 3] })
  })

  it('makes no call in online mode, with the setting off, or for an empty queue', async () => {
    const { useLyricsSource } = await import('./lyrics-source.js')
    const { applySkipChecked } = await import('./skip-checked.js')
    expect(await applySkipChecked([1, 2])).toEqual([1, 2])
    useLyricsSource().setForkConfig(localConfig(false))
    expect(await applySkipChecked([1, 2])).toEqual([1, 2])
    useLyricsSource().setForkConfig(localConfig())
    expect(await applySkipChecked([])).toEqual([])
    expect(invoke).not.toHaveBeenCalled()
  })

  it('keeps the queue when the backend answers with something odd', async () => {
    invoke.mockResolvedValue(undefined)
    const { useLyricsSource } = await import('./lyrics-source.js')
    useLyricsSource().setForkConfig(localConfig())
    const { applySkipChecked } = await import('./skip-checked.js')
    expect(await applySkipChecked([1])).toEqual([1])
  })

  it('downloads every track when the backend filter fails', async () => {
    invoke.mockRejectedValue('database is locked')
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const { useLyricsSource } = await import('./lyrics-source.js')
    useLyricsSource().setForkConfig(localConfig())
    const { applySkipChecked, skippedEverything } = await import('./skip-checked.js')
    expect(await applySkipChecked([1, 2, 3])).toEqual([1, 2, 3])
    expect(skippedEverything()).toBe(false)
    expect(warn).toHaveBeenCalled()
  })

  it('says when every track was filtered out, and lets the next run through once', async () => {
    invoke.mockResolvedValue([])
    const { useLyricsSource } = await import('./lyrics-source.js')
    useLyricsSource().setForkConfig(localConfig())
    const { applySkipChecked, skippedEverything, useSkipChecked } =
      await import('./skip-checked.js')
    expect(await applySkipChecked([1, 2, 3])).toEqual([])
    expect(skippedEverything()).toBe(true)
    const { allSkipped, downloadAnyway } = useSkipChecked()
    expect(allSkipped.value).toBe(3)

    downloadAnyway()
    expect(skippedEverything()).toBe(false)
    expect(await applySkipChecked([1, 2, 3])).toEqual([1, 2, 3])
    expect(invoke).toHaveBeenCalledTimes(1)

    expect(await applySkipChecked([1, 2, 3])).toEqual([], 'the bypass is used up')
    expect(invoke).toHaveBeenCalledTimes(2)
  })

  it('does not report a partly filtered queue as all skipped', async () => {
    invoke.mockResolvedValue([2])
    const { useLyricsSource } = await import('./lyrics-source.js')
    useLyricsSource().setForkConfig(localConfig())
    const { applySkipChecked, skippedEverything } = await import('./skip-checked.js')
    expect(await applySkipChecked([1, 2])).toEqual([2])
    expect(skippedEverything()).toBe(false)
  })
})
