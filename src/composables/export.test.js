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
  total: 4,
  done: 1,
  exported: 1,
  unchanged: 0,
  skipped: 0,
  errors: 0,
  pauseReason: null,
  ...overrides,
})

const entry = (id, status = 'exported') => ({
  id,
  status,
  title: `Song ${id}`,
  artistName: 'Artist',
  message: 'Exported 1 format(s)',
})

async function attach(snapshot) {
  let handler
  listen.mockImplementation(async (name, callback) => {
    expect(name).toBe('export-run')
    handler = callback
    return () => {}
  })
  invoke.mockImplementation(async command => (command === 'get_export_run' ? snapshot : null))
  const { useExporter } = await import('./export.js')
  const exporter = useExporter()
  await exporter.exportNext()
  return { exporter, emit: payload => handler({ payload }) }
}

describe('useExporter', () => {
  it('mirrors the snapshot and the batched events of the backend run', async () => {
    const { exporter, emit } = await attach({ run: run(), log: [entry(1)] })
    expect(exporter.isExporting.value).toBe(true)
    expect(exporter.isActive.value).toBe(true)
    expect(exporter.doneCount.value).toBe(1)
    expect(exporter.exportProgress.value).toBe(0.25)

    emit({
      run: run({ done: 3, exported: 2, unchanged: 1 }),
      entries: [entry(2), entry(3, 'unchanged')],
    })
    expect(exporter.unchangedCount.value).toBe(1)
    expect(exporter.log.value.map(item => item.id)).toEqual([3, 2, 1])

    // A track queued again by Retry replaces its old line.
    emit({ run: run({ done: 3 }), entries: [{ ...entry(1), status: 'error' }] })
    expect(exporter.log.value.map(item => item.id)).toEqual([1, 3, 2])

    emit({ run: run({ done: 4, status: 'finished' }), entries: [entry(4)] })
    expect(exporter.isFinished.value).toBe(true)
    expect(exporter.isActive.value).toBe(false)
  })

  it('ignores events older than what it holds and keeps a hundred lines', async () => {
    const { exporter, emit } = await attach({ run: run(), log: [] })
    const newest = run({ done: 2 })
    emit({ run: newest, entries: [entry(2)] })
    emit({ run: { ...newest, revision: newest.revision - 1 }, entries: [entry(9)] })
    expect(exporter.log.value.map(item => item.id)).toEqual([2])
    const many = Array.from({ length: 120 }, (_, index) => entry(index + 10))
    emit({ run: run(), entries: many })
    expect(exporter.log.value).toHaveLength(100)
    expect(exporter.log.value[0].id).toBe(129)
  })

  it('asks for attention when the backend reports an interrupted run', async () => {
    const { exporter } = await attach({
      run: run({ status: 'paused', pauseReason: 'interrupted' }),
      log: [],
    })
    expect(exporter.needsAttention.value).toBe(true)
    expect(exporter.isInterrupted.value).toBe(true)
    expect(exporter.isActive.value).toBe(false)
  })

  it('is not finished before a run exists', async () => {
    const { exporter } = await attach({
      run: { revision: 0, exists: false, status: 'none' },
      log: [],
    })
    expect(exporter.isFinished.value).toBe(false)
    expect(exporter.isExporting.value).toBe(false)
  })

  it('pauses, resumes, retries and clears through the backend', async () => {
    const { exporter } = await attach({ run: run(), log: [entry(1)] })
    let latest = run()
    invoke.mockImplementation(async command => {
      if (command === 'get_export_run') return { run: { ...latest }, log: [] }
      if (command === 'clear_export_run') {
        latest = run({ exists: false, status: 'none' })
      } else {
        latest = run({ status: command === 'pause_export_run' ? 'paused' : 'running' })
      }
      return { ...latest }
    })
    await exporter.pauseExporting()
    expect(exporter.isPaused.value).toBe(true)
    await exporter.resumeExporting()
    expect(exporter.isActive.value).toBe(true)
    await exporter.retryErrors()
    await exporter.startOver()
    expect(exporter.isExporting.value).toBe(false)
    expect(exporter.log.value).toEqual([])
    expect(invoke.mock.calls.map(([name]) => name)).toEqual([
      'get_export_run',
      'pause_export_run',
      'get_export_run',
      'resume_export_run',
      'retry_export_errors',
      'get_export_run',
      'clear_export_run',
    ])
  })
})
