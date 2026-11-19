// @vitest-environment jsdom
import { act, renderHook, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { useLibrary } from './useLibrary'
import type { LibraryTrack } from '@/lib/tauri'

const invoke = vi.fn()
vi.mock('@/lib/tauri', () => ({ invoke: (...args: unknown[]) => invoke(...args) }))

const track = { id: 'imported-mp3', title: 'Song', artist: 'Artist', duration: 225000 } as LibraryTrack
const missingKey = "invalid args 'trackId' for command 'library_register_track': command library_register_track missing required key trackId"

beforeEach(() => {
  invoke.mockReset()
  invoke.mockImplementation((command: string, args?: Record<string, unknown>) => {
    if (command === 'library_register_track' && !args?.trackId) return Promise.reject(missingKey)
    return Promise.resolve(command.startsWith('library_get_') ? [] : undefined)
  })
})

describe('library playback IPC', () => {
  it('reproduces the original missing trackId error for snake_case arguments', async () => {
    await expect(invoke('library_register_track', { track_id: track.id })).rejects.toBe(missingKey)
  })

  it('registers the same track before playing with the frontend payload', async () => {
    const { result } = renderHook(() => useLibrary())
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('library_get_tracks'))
    await act(async () => { await result.current.playTrack(track) })
    const calls = invoke.mock.calls.filter(([command]) =>
      command === 'library_register_track' || command === 'audio_play')
    expect(calls).toEqual([
      ['library_register_track', { trackId: track.id }],
      ['audio_play', { trackHandle: {
        id: track.id, title: track.title, artist: track.artist, duration: 225000,
      } }],
    ])
    expect(result.current.error).toBeNull()
  })

  it('shows registration failure and never invokes playback', async () => {
    invoke.mockImplementation((command: string) =>
      command === 'library_register_track' ? Promise.reject(missingKey) : Promise.resolve([]))
    const { result } = renderHook(() => useLibrary())
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('library_get_tracks'))
    await act(async () => {
      await expect(result.current.playTrack(track)).rejects.toBe(missingKey)
    })
    expect(result.current.error).toBe(missingKey)
    expect(invoke.mock.calls.some(([command]) => command === 'audio_play')).toBe(false)
  })

  it('shows playback failure after successful registration', async () => {
    invoke.mockImplementation((command: string) =>
      command === 'audio_play' ? Promise.reject('playback failed') : Promise.resolve([]))
    const { result } = renderHook(() => useLibrary())
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('library_get_tracks'))
    await act(async () => {
      await expect(result.current.playTrack(track)).rejects.toBe('playback failed')
    })
    expect(result.current.error).toBe('playback failed')
  })
})
