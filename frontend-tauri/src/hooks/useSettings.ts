import { useCallback, useEffect } from 'react'
import { invoke } from '@/lib/tauri'
import { useSettingsStore } from '@/lib/stores'
import type { Settings } from '@/lib/tauri'

export function useSettings() {
  const { settings, loading, setSettings, setLoading } = useSettingsStore()

  // Load settings on mount
  useEffect(() => {
    async function load() {
      try {
        setLoading(true)
        const data = await invoke<Settings>('settings_get_all')
        setSettings(data)
      } catch (error) {
        console.error('Failed to load settings:', error)
      } finally {
        setLoading(false)
      }
    }

    load()
  }, [setSettings, setLoading])

  const update = useCallback(async <K extends keyof Settings>(key: K, value: Settings[K]) => {
    try {
      const current = await invoke<Settings>('settings_get_all')
      const updated = { ...current, [key]: value }
      await invoke('settings_set', { key: 'settings', value: updated })
      setSettings(updated)
    } catch (error) {
      console.error(`Failed to update setting ${String(key)}:`, error)
      throw error
    }
  }, [setSettings])

  const remove = useCallback(async (key: string) => {
    try {
      await invoke('settings_remove', { key })
    } catch (error) {
      console.error(`Failed to remove setting ${key}:`, error)
      throw error
    }
  }, [])

  const reset = useCallback(async () => {
    try {
      await invoke('settings_reset')
      // Reload settings
      const data = await invoke<Settings>('settings_get_all')
      setSettings(data)
    } catch (error) {
      console.error('Failed to reset settings:', error)
      throw error
    }
  }, [setSettings])

  return {
    settings,
    loading,
    update,
    remove,
    reset,
  }
}
