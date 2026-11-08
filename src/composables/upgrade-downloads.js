// Lavender: the Upgrade mass download. The choice lives in this browser's
// storage because upstream's settings only know the three skip flags.

const KEY = 'lavender.upgradeDownloads'

export const isUpgradeMode = mode => mode === 'upgrade'

export const upgradeRemembered = () => {
  try {
    return localStorage.getItem(KEY) === '1'
  } catch {
    return false
  }
}

export const rememberUpgrade = on => {
  try {
    if (on) {
      localStorage.setItem(KEY, '1')
    } else {
      localStorage.removeItem(KEY)
    }
  } catch {
    // Storage may be unavailable; the choice is then not remembered.
  }
}
