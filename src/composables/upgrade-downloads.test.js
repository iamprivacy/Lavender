import { beforeEach, describe, expect, it, vi } from 'vitest'
import { isUpgradeMode, rememberUpgrade, upgradeRemembered } from './upgrade-downloads.js'

const storage = new Map()

beforeEach(() => {
  storage.clear()
  vi.stubGlobal('localStorage', {
    getItem: key => (storage.has(key) ? storage.get(key) : null),
    setItem: (key, value) => storage.set(key, String(value)),
    removeItem: key => storage.delete(key),
  })
})

describe('upgrade downloads', () => {
  it('knows its own mode', () => {
    expect(isUpgradeMode('upgrade')).toBe(true)
    expect(isUpgradeMode('all')).toBe(false)
  })

  it('remembers the choice on this machine', () => {
    expect(upgradeRemembered()).toBe(false)
    rememberUpgrade(true)
    expect(upgradeRemembered()).toBe(true)
    rememberUpgrade(false)
    expect(upgradeRemembered()).toBe(false)
  })

  it('survives storage that throws', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('blocked')
      },
      setItem: () => {
        throw new Error('blocked')
      },
      removeItem: () => {
        throw new Error('blocked')
      },
    })
    expect(() => rememberUpgrade(true)).not.toThrow()
    expect(upgradeRemembered()).toBe(false)
  })
})
