import { invoke } from '@tauri-apps/api/core'

// Lavender: the Search Lyrics dialog asks the backend which of its rows the
// mass download would take for the track, best first. Search rows carry no
// lyrics text since upstream 2.2.2 (v2 search), so only the availability
// flags go along; the backend reads the texts from the local database for the
// content score and the edition rule. It answers only in Local mode; in Online
// mode the list is empty and nothing moves.

export const rankSearchResults = async (track, items) => {
  if (!track || !Array.isArray(items) || items.length === 0) {
    return []
  }
  try {
    const ranked = await invoke('rank_search_results', {
      trackId: track.id,
      rows: items.map(item => ({
        id: item.id,
        name: item.trackName,
        artistName: item.artistName,
        albumName: item.albumName,
        duration: item.duration,
        instrumental: Boolean(item.instrumental),
        synced: Boolean(item.hasSyncedLyrics),
        plain: Boolean(item.hasPlainLyrics),
      })),
    })
    return Array.isArray(ranked) ? ranked : []
  } catch (error) {
    console.error(error)
    return []
  }
}

// The ranked rows first, in rank order, then the rest as they came.
export const orderByRank = (items, rankedIds) => {
  if (!Array.isArray(items)) {
    return []
  }
  const rank = new Map((rankedIds || []).map((id, index) => [id, index]))
  const position = item => (rank.has(item.id) ? rank.get(item.id) : rank.size)
  return items
    .map((item, index) => ({ item, index }))
    .sort((a, b) => position(a.item) - position(b.item) || a.index - b.index)
    .map(({ item }) => item)
}
