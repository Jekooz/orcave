// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { MediaItemCard } from './MediaItemCard'
import type { MediaItem } from '@/lib/tauri'

const invoke = vi.fn()
vi.mock('@/lib/tauri', () => ({ invoke: (...args: unknown[]) => invoke(...args) }))

const item = {
  id: 'imported-mp3', media_type: 'track', title: 'Song',
  artists: [{ name: 'Artist', id: 'artist-1' }], thumbnails: [],
  is_explicit: false, is_liked: false, is_in_library: true, metadata: {},
} satisfies MediaItem
const missingKey = "invalid args 'trackId' for command 'library_register_track': command library_register_track missing required key trackId"

beforeEach(() => { invoke.mockReset() })
afterEach(cleanup)

it('registers the same ID before Play with camelCase command arguments', async () => {
  invoke.mockResolvedValue(undefined)
  render(<MediaItemCard item={item} />)
  fireEvent.click(screen.getByRole('button', { name: 'Play Song' }))
  await waitFor(() => expect(invoke).toHaveBeenCalledTimes(2))
  expect(invoke.mock.calls).toEqual([
    ['library_register_track', { trackId: item.id }],
    ['audio_play', { trackHandle: { id: item.id, title: item.title, artist: 'Artist' } }],
  ])
})

it('displays registration errors and skips Play', async () => {
  invoke.mockRejectedValue(missingKey)
  render(<MediaItemCard item={item} />)
  fireEvent.click(screen.getByRole('button', { name: 'Play Song' }))
  expect((await screen.findByRole('status')).textContent).toContain(missingKey)
  expect(invoke).toHaveBeenCalledTimes(1)
})
