import { useCallback, useEffect } from 'react'
import { invoke, listen } from '@/lib/tauri'
import { useQueueStore } from '@/lib/stores'
import type { QueueItem, QueueMode, RepeatMode, SeedType, MediaItem, TrackHandle } from '@/lib/tauri'

export function useQueue() {
  const { queue, history, currentIndex, mode, repeatMode, shuffle, setQueue, setHistory, setCurrentIndex, setMode, setRepeatMode, setShuffle } = useQueueStore()

  // Listen for queue events
  useEffect(() => {
    let unsubscribe: (() => void) | null = null

    async function init() {
      // Load initial queue
      try {
        const [queueData, historyData] = await Promise.all([
          invoke<QueueItem[]>('queue_get'),
          invoke<QueueItem[]>('queue_get_history'),
        ])
        setQueue(queueData)
        setHistory(historyData)
      } catch (error) {
        console.error('Failed to load queue:', error)
      }

      unsubscribe = await listen<{ type: string; data: unknown }>('queue_event', (event) => {
        const { type, data } = event.payload
        switch (type) {
          case 'item_added':
            // Queue is refreshed from backend
            invoke<QueueItem[]>('queue_get').then(setQueue).catch(console.error)
            break
          case 'item_removed':
            invoke<QueueItem[]>('queue_get').then(setQueue).catch(console.error)
            break
          case 'items_reordered':
            invoke<QueueItem[]>('queue_get').then(setQueue).catch(console.error)
            break
          case 'cleared':
            setQueue([])
            break
          case 'current_index_changed':
            setCurrentIndex((data as { index: number }).index)
            break
          case 'mode_changed':
            setMode(data as QueueMode)
            break
          case 'repeat_mode_changed':
            setRepeatMode(data as RepeatMode)
            break
          case 'shuffle_changed':
            setShuffle(data as boolean)
            break
        }
      })
    }

    init()

    return () => {
      unsubscribe?.()
    }
  }, [setQueue, setHistory, setCurrentIndex, setMode, setRepeatMode, setShuffle])

  const add = useCallback(async (mediaItem: MediaItem, trackHandle: TrackHandle, source: QueueItem['source'] = 'user_added') => {
    try {
      const result = await invoke<number>('queue_add', { mediaItem, trackHandle, source })
      setQueue(await invoke<QueueItem[]>('queue_get'))
      return result
    } catch (error) {
      console.error('Add to queue failed:', error)
      throw error
    }
  }, [setQueue])

  const remove = useCallback(async (index: number) => {
    try {
      const result = await invoke<QueueItem>('queue_remove', { index })
      setQueue(await invoke<QueueItem[]>('queue_get'))
      return result
    } catch (error) {
      console.error('Remove from queue failed:', error)
      throw error
    }
  }, [setQueue])

  const move = useCallback(async (from: number, to: number) => {
    try {
      await invoke('queue_move', { from, to })
      setQueue(await invoke<QueueItem[]>('queue_get'))
    } catch (error) {
      console.error('Move in queue failed:', error)
      throw error
    }
  }, [setQueue])

  const clear = useCallback(async () => {
    try {
      await invoke('queue_clear')
      setQueue([])
    } catch (error) {
      console.error('Clear queue failed:', error)
      throw error
    }
  }, [setQueue])

  const playIndex = useCallback(async (index: number) => {
    try {
      await invoke('queue_play_index', { index })
    } catch (error) {
      console.error('Play index failed:', error)
      throw error
    }
  }, [])

  const next = useCallback(async () => {
    try {
      await invoke('queue_next')
    } catch (error) {
      console.error('Next failed:', error)
    }
  }, [])

  const previous = useCallback(async () => {
    try {
      await invoke('queue_previous')
    } catch (error) {
      console.error('Previous failed:', error)
    }
  }, [])

  const setRepeat = useCallback(async (mode: RepeatMode) => {
    try {
      await invoke('queue_set_repeat', { mode })
    } catch (error) {
      console.error('Set repeat failed:', error)
    }
  }, [])

  const setShuffleMode = useCallback(async (enabled: boolean) => {
    try {
      await invoke('queue_set_shuffle', { enabled })
    } catch (error) {
      console.error('Set shuffle failed:', error)
    }
  }, [])

  const startRadio = useCallback(async (seedType: SeedType, seedId: string) => {
    try {
      await invoke('queue_start_radio', { seedType, seedId })
    } catch (error) {
      console.error('Start radio failed:', error)
      throw error
    }
  }, [])

  const currentItem = queue[currentIndex] || null

  return {
    queue,
    history,
    currentIndex,
    currentItem,
    mode,
    repeatMode,
    shuffle,
    add,
    remove,
    move,
    clear,
    playIndex,
    next,
    previous,
    setRepeat,
    setShuffle: setShuffleMode,
    startRadio,
  }
}
