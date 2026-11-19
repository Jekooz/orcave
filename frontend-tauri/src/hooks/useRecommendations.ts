import { useCallback } from 'react'
import { invoke } from '@/lib/tauri'
import { useRecommendationsStore } from '@/lib/stores'
import type { SeedType, MixType, StationType, RecommendationSet, MediaItem } from '@/lib/tauri'

export function useRecommendations() {
  const { currentSet, mixes, stations, loading, setCurrentSet, setMix, setStation, setLoading } = useRecommendationsStore()

  const getForSeed = useCallback(async (seedType: SeedType, seedId: string, limit = 20) => {
    setLoading(true)
    try {
      const items = await invoke<MediaItem[]>('recommendations_get_for_seed', {
        seedType,
        seedId,
        limit,
      })
      const set: RecommendationSet = {
        id: crypto.randomUUID(),
        title: `${seedType} Recommendations`,
        subtitle: seedId,
        seed_type: seedType,
        seed_id: seedId,
        items,
        generated_at: new Date().toISOString(),
        expires_at: new Date(Date.now() + 30 * 60 * 1000).toISOString(),
        algorithm: 'seed_based',
      }
      setCurrentSet(set)
      return set
    } catch (error) {
      console.error('Get recommendations failed:', error)
      throw error
    } finally {
      setLoading(false)
    }
  }, [setCurrentSet, setLoading])

  const getMix = useCallback(async (mixType: MixType, limit = 50) => {
    setLoading(true)
    try {
      const set = await invoke<RecommendationSet>('recommendations_get_mix', {
        mixType,
        limit,
      })
      setMix(mixType, set)
      return set
    } catch (error) {
      console.error('Get mix failed:', error)
      throw error
    } finally {
      setLoading(false)
    }
  }, [setMix, setLoading])

  const getStation = useCallback(async (stationType: StationType, limit = 50) => {
    setLoading(true)
    try {
      const set = await invoke<RecommendationSet>('recommendations_get_station', {
        stationType,
        limit,
      })
      setStation(stationType, set)
      return set
    } catch (error) {
      console.error('Get station failed:', error)
      throw error
    } finally {
      setLoading(false)
    }
  }, [setStation, setLoading])

  const getRelated = useCallback(async (item: MediaItem, limit = 20) => {
    try {
      return await invoke<MediaItem[]>('recommendations_get_related', { item, limit })
    } catch (error) {
      console.error('Get related failed:', error)
      throw error
    }
  }, [])

  return {
    currentSet,
    mixes,
    stations,
    loading,
    getForSeed,
    getMix,
    getStation,
    getRelated,
  }
}
