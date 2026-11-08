import { beforeEach, describe, expect, it, vi } from 'vitest'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }))

beforeEach(() => {
  vi.resetModules()
  vi.resetAllMocks()
  listen.mockResolvedValue(() => {})
})

const available = {
  status: 'available',
  currentVersion: '2.2.0-local.1',
  latestVersion: '2.2.0-local.2',
  releaseUrl: 'https://github.com/iamprivacy/Lavender/releases/tag/v2.2.0-local.2',
  canInstall: true,
  installBlock: null,
  dismissed: false,
  error: null,
}

describe('progressPercent and phaseText', () => {
  it('describes each install phase', async () => {
    const { progressPercent, phaseText } = await import('./app-update.js')
    expect(progressPercent(null)).toBe(null)
    expect(progressPercent({ downloaded: 5, total: null })).toBe(null)
    expect(progressPercent({ downloaded: 50, total: 200 })).toBe(25)
    expect(progressPercent({ downloaded: 300, total: 200 })).toBe(100)
    expect(phaseText({ phase: 'downloading', downloaded: 1, total: 4 })).toBe('Downloading... 25%')
    expect(phaseText({ phase: 'downloading', downloaded: 1, total: null })).toBe('Downloading...')
    expect(phaseText({ phase: 'failed', message: 'Refusing to install an update: x.' })).toBe(
      'Install failed: Refusing to install an update: x.'
    )
    expect(phaseText({ phase: 'ready', message: 'Restart to finish.' })).toBe('Restart to finish.')
    expect(phaseText(null)).toBe('')
  })
})

describe('useAppUpdate', () => {
  it('offers the toast after an automatic check finds a new release', async () => {
    invoke.mockImplementation(async command => {
      if (command === 'startup_app_update_check') return available
    })
    const { useAppUpdate } = await import('./app-update.js')
    const update = useAppUpdate()
    await update.startupCheck()
    expect(update.toast.value).toEqual({ version: '2.2.0-local.2' })
    expect(update.check.value).toEqual(available)
  })

  it('stays quiet when no check was due or the release was dismissed', async () => {
    invoke.mockResolvedValueOnce(null)
    const { useAppUpdate } = await import('./app-update.js')
    const update = useAppUpdate()
    await update.startupCheck()
    expect(update.toast.value).toBe(null)

    invoke.mockResolvedValueOnce({ ...available, dismissed: true })
    await update.startupCheck()
    expect(update.toast.value).toBe(null)
  })

  it('a manual check never opens the toast', async () => {
    invoke.mockResolvedValueOnce(available)
    const { useAppUpdate } = await import('./app-update.js')
    const update = useAppUpdate()
    await update.checkNow()
    expect(update.check.value.status).toBe('available')
    expect(update.toast.value).toBe(null)
    expect(update.checking.value).toBe(false)
  })

  it('closing the toast remembers the release', async () => {
    invoke.mockImplementation(async command => {
      if (command === 'startup_app_update_check') return available
    })
    const { useAppUpdate } = await import('./app-update.js')
    const update = useAppUpdate()
    await update.startupCheck()
    await update.dismissToast()
    expect(update.toast.value).toBe(null)
    expect(invoke).toHaveBeenCalledWith('dismiss_app_update', { version: '2.2.0-local.2' })
  })

  it('follows install progress events and reports a refused start', async () => {
    let handler
    listen.mockImplementation(async (name, h) => {
      expect(name).toBe('app-update-progress')
      handler = h
      return () => {}
    })
    invoke.mockRejectedValueOnce('An update is already being installed.')
    const { useAppUpdate } = await import('./app-update.js')
    const update = useAppUpdate()
    await update.install()
    expect(update.progress.value).toEqual({
      phase: 'failed',
      message: 'An update is already being installed.',
    })

    handler({ payload: { phase: 'downloading', downloaded: 10, total: 20 } })
    expect(update.busy.value).toBe(true)
    handler({ payload: { phase: 'ready', version: '2.2.0-local.2', message: 'Restart.' } })
    expect(update.busy.value).toBe(false)
    expect(update.ready.value).toBe(true)
    expect(listen).toHaveBeenCalledTimes(1)
  })

  it('answering the prompt checks right away when accepted', async () => {
    invoke.mockImplementation(async command => {
      if (command === 'resolve_app_update_opt_in') return available
      if (command === 'get_app_update_info') {
        return { currentVersion: '2.2.0-local.1', prefs: { autoCheck: true }, shouldPrompt: false }
      }
    })
    const { useAppUpdate } = await import('./app-update.js')
    const update = useAppUpdate()
    await update.resolveOptIn(true)
    expect(invoke).toHaveBeenCalledWith('resolve_app_update_opt_in', { accept: true })
    expect(update.toast.value).toEqual({ version: '2.2.0-local.2' })
    expect(update.info.value.shouldPrompt).toBe(false)
  })
})
