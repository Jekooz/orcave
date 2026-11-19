import { useCallback, useEffect } from 'react'
import { invoke, listen } from '@/lib/tauri'
import { useCatalogueStore } from '@/lib/stores'
import type { MediaItem, BrowseResponse, SearchResponse } from '@/lib/tauri'

export function useCatalogue() {
  const { currentBrowse, searchResults, searchLoading, browseLoading, setCurrentBrowse, setSearchResults, setSearchLoading, setBrowseLoading } = useCatalogueStore()

  // Listen for catalogue events
  useEffect(() => {
    let unsubscribeBrowse: (() => void) | null = null
    let unsubscribeSearch: (() => void) | null = null

    async function init() {
      unsubscribeBrowse = await listen<BrowseResponse>('catalogue_browse_completed', (event) => {
        setCurrentBrowse(event.payload)
        setBrowseLoading(false)
      })

      unsubscribeSearch = await listen<SearchResponse>('catalogue_search_completed', (event) => {
        setSearchResults(event.payload)
        setSearchLoading(false)
      })
    }

    init()

    return () => {
      unsubscribeBrowse?.()
      unsubscribeSearch?.()
    }
  }, [setCurrentBrowse, setSearchResults, setSearchLoading, setBrowseLoading])

  const browse = useCallback(async (browseId: string) => {
    setBrowseLoading(true)
    try {
       const result = await invoke<BrowseResponse>('catalogue_browse', { browseId })
      setCurrentBrowse(result)
      setBrowseLoading(false)
      return result
    } catch (error) {
      console.error('Browse failed:', error)
      setBrowseLoading(false)
      throw error
    }
  }, [setCurrentBrowse, setBrowseLoading])

  const search = useCallback(async (query: string, filter?: string) => {
    setSearchLoading(true)
    try {
      const result = await invoke<SearchResponse>('catalogue_search', { query, filter })
      setSearchResults(result)
      setSearchLoading(false)
      return result
    } catch (error) {
      console.error('Search failed:', error)
      setSearchLoading(false)
      throw error
    }
  }, [setSearchResults, setSearchLoading])

  const getItem = useCallback(async (itemId: string) => {
    try {
       return await invoke<MediaItem>('catalogue_get_item', { itemId })
    } catch (error) {
      console.error('Get item failed:', error)
      throw error
    }
  }, [])

  const getStreamUrl = useCallback(async (videoId: string, quality: 'low' | 'normal' | 'high' | 'lossless') => {
    try {
       return await invoke<string>('catalogue_get_stream_url', { videoId, quality })
    } catch (error) {
      console.error('Get stream URL failed:', error)
      throw error
    }
  }, [])

  return {
    currentBrowse,
    searchResults,
    searchLoading,
    browseLoading,
    browse,
    search,
    getItem,
    getStreamUrl,
  }
}

// Predefined browse IDs
export const BROWSE_IDS = {
  home: 'FEmusic_home',
  library: 'FEmusic_library',
  explore: 'FEmusic_explore',
  liked_music: 'FEmusic_liked_music',
  history: 'FEmusic_history',
  downloads: 'FEmusic_downloads',
} as const
