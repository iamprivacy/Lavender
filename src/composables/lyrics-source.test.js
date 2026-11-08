import { beforeEach, describe, expect, it, vi } from 'vitest'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }))

beforeEach(() => {
  vi.resetModules()
  vi.resetAllMocks()
})

describe('humanSize', () => {
  it('formats bytes with decimal units', async () => {
    const { humanSize } = await import('./lyrics-source.js')
    expect(humanSize(512)).toBe('512 B')
    expect(humanSize(47837763733)).toBe('47.8 GB')
    expect(humanSize(1500)).toBe('1.5 KB')
    expect(humanSize(null)).toBe('')
  })
})

describe('humanSpeed and parentFolder', () => {
  it('formats a rate and finds the folder of a path', async () => {
    const { humanSpeed, parentFolder } = await import('./lyrics-source.js')
    expect(humanSpeed(0)).toBe('')
    expect(humanSpeed(25_600_000)).toBe('25.6 MB/s')
    expect(parentFolder('/Volumes/Big/lrclib.sqlite3')).toBe('/Volumes/Big')
    expect(parentFolder('D:\\Dumps\\lrclib.sqlite3')).toBe('D:\\Dumps')
    expect(parentFolder('lrclib.sqlite3')).toBe('')
    expect(parentFolder(null)).toBe('')
  })
})

describe('useLyricsSource', () => {
  it('loads the fork config and status, and listens for status events once', async () => {
    let statusHandler
    listen.mockImplementation(async (name, handler) => {
      expect(name).toBe('local-db-status')
      statusHandler = handler
      return () => {}
    })
    invoke.mockImplementation(async command => {
      if (command === 'get_fork_config') {
        return { lyricsSource: 'local', localDbPath: '/x/dump.sqlite3', fallbackOnline: false }
      }
      if (command === 'get_local_db_status') {
        return {
          configuredPath: '/x/dump.sqlite3',
          open: true,
          info: { trackCount: 3 },
          error: null,
        }
      }
      throw new Error(`unexpected command ${command}`)
    })

    const { useLyricsSource } = await import('./lyrics-source.js')
    const source = useLyricsSource()
    expect(source.isLocal.value).toBe(false)
    expect(source.forkConfig.value.extraAmll).toBe(false)
    expect(source.forkConfig.value.extraApple).toBe(false)
    expect(source.forkConfig.value.appleUserToken).toBeNull()
    expect(source.forkConfig.value.extraNetease).toBe(false)
    expect(source.forkConfig.value.extraQq).toBe(false)
    expect(source.forkConfig.value.extraPlainLyrics).toBe(false)
    expect(source.forkConfig.value.exportWordTimestamps).toBe(false)
    expect(source.forkConfig.value.lyricsEdition).toBe('follow-file')

    await source.loadLyricsSource()
    expect(source.isLocal.value).toBe(true)
    expect(source.lyricsSource.value).toBe('local')
    expect(source.localDbStatus.value.info.trackCount).toBe(3)

    await source.loadLyricsSource()
    expect(listen).toHaveBeenCalledTimes(1)

    statusHandler({
      payload: { configuredPath: '/x/dump.sqlite3', open: false, info: null, error: 'gone' },
    })
    expect(source.localDbStatus.value.open).toBe(false)
    expect(source.localDbStatus.value.error).toBe('gone')
  })

  it('saves through set_fork_config and keeps what the backend returned', async () => {
    invoke.mockImplementation(async (command, args) => {
      expect(command).toBe('set_fork_config')
      return { ...args.forkConfig, localDbPath: null }
    })
    const { useLyricsSource } = await import('./lyrics-source.js')
    const source = useLyricsSource()
    const saved = await source.saveForkConfig({
      lyricsSource: 'online',
      localDbPath: '   ',
      fallbackOnline: true,
      extraNetease: true,
      extraQq: true,
      extraPlainLyrics: false,
    })
    expect(saved.localDbPath).toBeNull()
    expect(source.forkConfig.value.lyricsSource).toBe('online')
    expect(invoke).toHaveBeenCalledWith('set_fork_config', {
      forkConfig: expect.objectContaining({
        extraNetease: true,
        extraQq: true,
        extraPlainLyrics: false,
      }),
    })
    expect(source.forkConfig.value.extraNetease).toBe(true)
    expect(source.forkConfig.value.extraQq).toBe(true)
  })
})

describe('saveForkConfig', () => {
  it('remembers why a save failed until one succeeds', async () => {
    const { useLyricsSource } = await import('./lyrics-source.js')
    const source = useLyricsSource()
    invoke.mockRejectedValueOnce('the database is locked')
    await expect(source.saveForkConfig({ lyricsSource: 'local' })).rejects.toBe(
      'the database is locked'
    )
    expect(source.saveError.value).toBe('the database is locked')
    expect(source.forkConfig.value.lyricsSource).toBe('online', 'the failed config is not applied')

    invoke.mockResolvedValueOnce({ lyricsSource: 'local' })
    await source.saveForkConfig({ lyricsSource: 'local' })
    expect(source.saveError.value).toBeNull()
    expect(source.forkConfig.value.lyricsSource).toBe('local')
  })
})
