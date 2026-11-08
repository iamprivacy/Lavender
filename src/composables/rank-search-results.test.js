import { beforeEach, describe, expect, it, vi } from 'vitest'
import { invoke } from '@tauri-apps/api/core'
import { orderByRank, rankSearchResults } from './rank-search-results.js'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))

beforeEach(() => {
  vi.resetAllMocks()
})

describe('rankSearchResults', () => {
  it('sends the rows the backend ranks on and returns its order', async () => {
    invoke.mockResolvedValue([3, 1])
    const items = [
      {
        id: 1,
        trackName: 'Wrath',
        artistName: 'Lorna Shore',
        albumName: 'Pain Remains',
        duration: 280,
        hasSyncedLyrics: true,
        hasPlainLyrics: true,
        hasLyricsfile: true,
      },
      {
        id: 3,
        trackName: 'Wrath',
        artistName: 'Lorna Shore',
        albumName: 'Pain Remains',
        duration: 280,
        instrumental: true,
        hasSyncedLyrics: false,
        hasPlainLyrics: false,
      },
    ]
    expect(await rankSearchResults({ id: 9 }, items)).toEqual([3, 1])
    expect(invoke).toHaveBeenCalledWith('rank_search_results', {
      trackId: 9,
      rows: [
        {
          id: 1,
          name: 'Wrath',
          artistName: 'Lorna Shore',
          albumName: 'Pain Remains',
          duration: 280,
          instrumental: false,
          synced: true,
          plain: true,
        },
        {
          id: 3,
          name: 'Wrath',
          artistName: 'Lorna Shore',
          albumName: 'Pain Remains',
          duration: 280,
          instrumental: true,
          synced: false,
          plain: false,
        },
      ],
    })
  })

  it('asks nothing without a track or rows and swallows a failure', async () => {
    expect(await rankSearchResults(null, [{ id: 1 }])).toEqual([])
    expect(await rankSearchResults({ id: 9 }, [])).toEqual([])
    expect(invoke).not.toHaveBeenCalled()
    invoke.mockRejectedValue(new Error('no'))
    vi.spyOn(console, 'error').mockImplementation(() => {})
    expect(await rankSearchResults({ id: 9 }, [{ id: 1 }])).toEqual([])
  })
})

describe('orderByRank', () => {
  it('puts the ranked rows first in rank order and keeps the rest as they came', () => {
    const items = [{ id: 1 }, { id: 2 }, { id: 3 }, { id: 4 }]
    expect(orderByRank(items, [3, 1]).map(item => item.id)).toEqual([3, 1, 2, 4])
    expect(orderByRank(items, []).map(item => item.id)).toEqual([1, 2, 3, 4])
    expect(orderByRank(null, [1])).toEqual([])
  })
})
