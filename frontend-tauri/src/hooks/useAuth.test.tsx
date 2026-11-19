// @vitest-environment jsdom
import { act, renderHook, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { useAuth } from './useAuth'
import { useAuthStore } from '@/lib/stores'

const invoke = vi.fn()
vi.mock('@/lib/tauri', () => ({ invoke: (...args: unknown[]) => invoke(...args) }))

const localSession = { state: 'connected', connected: true, expired: false, ttl_secs: 3600, handle: 'redacted' }
const unverifiedSession = { state: 'unverified', connected: false, expired: false, ttl_secs: 0, handle: null }

beforeEach(() => {
  invoke.mockReset()
  useAuthStore.setState({ state: 'authenticated', user: { id: 'oauth', name: 'OAuth' } as never, loading: false })
})

describe('native session status', () => {
  it.each([
    [localSession, 'local_session'],
    [unverifiedSession, 'local_session'],
    [{ ...unverifiedSession, connected: true }, 'error'],
    [{ ...localSession, connected: false }, 'error'],
    [{ ...localSession, handle: null }, 'error'],
    [{ ...localSession, ttl_secs: 0 }, 'error'],
    [{ ...localSession, expired: true }, 'expired'],
    [{ ...localSession, state: 'disconnected', connected: false, handle: null }, 'disconnected'],
    [{ ...localSession, state: 'untrusted' }, 'error'],
  ])('does not authenticate from native response %j', async (response, expected) => {
    invoke.mockResolvedValue(response)
    const { result } = renderHook(() => useAuth())
    await waitFor(() => expect(result.current.nativeStatus).toBe(expected))
    expect(result.current.isAuthenticated).toBe(false)
    expect(useAuthStore.getState().state).not.toBe('authenticated')
    expect(useAuthStore.getState().user).toBeNull()
    expect(invoke).toHaveBeenCalledWith('native_session_get_status')
    expect(invoke.mock.calls.some(([name]) => name.startsWith('auth_'))).toBe(false)
  })

  it('fails closed when native status IPC fails', async () => {
    invoke.mockRejectedValue(new Error('secret credential'))
    const { result } = renderHook(() => useAuth())
    await waitFor(() => expect(result.current.actionError).toBe(true))
    expect(result.current.nativeStatus).toBe('error')
    expect(result.current.isAuthenticated).toBe(false)
    expect(useAuthStore.getState().state).toBe('error')
  })

  it('disconnects only through native IPC and rechecks status', async () => {
    invoke.mockImplementation((name: string) => {
      if (name === 'native_session_get_status')
        return Promise.resolve(invoke.mock.calls.filter(([call]) => call === name).length === 1
          ? localSession
          : { state: 'disconnected', connected: false, expired: false, ttl_secs: 0, handle: null })
      if (name === 'native_session_disconnect') return Promise.resolve()
    })
    const { result } = renderHook(() => useAuth())
    await waitFor(() => expect(result.current.nativeStatus).toBe('local_session'))
    await act(async () => { await result.current.signOut() })
    expect(result.current.nativeStatus).toBe('disconnected')
    expect(invoke.mock.calls.map(([name]) => name)).toEqual([
      'native_session_get_status', 'native_session_disconnect', 'native_session_get_status',
    ])
  })

  it('lets an unverified retained session disconnect without enabling provider access', async () => {
    invoke.mockImplementation((name: string) => {
      if (name === 'native_session_get_status')
        return Promise.resolve(invoke.mock.calls.filter(([call]) => call === name).length === 1
          ? unverifiedSession
          : { state: 'disconnected', connected: false, expired: false, ttl_secs: 0, handle: null })
      if (name === 'native_session_disconnect') return Promise.resolve()
    })
    const { result } = renderHook(() => useAuth())
    await waitFor(() => expect(result.current.nativeStatus).toBe('local_session'))
    expect(result.current.isAuthenticated).toBe(false)
    expect(useAuthStore.getState().state).toBe('unauthenticated')
    await act(async () => { await result.current.signOut() })
    expect(result.current.nativeStatus).toBe('disconnected')
    expect(invoke.mock.calls.map(([name]) => name)).toEqual([
      'native_session_get_status', 'native_session_disconnect', 'native_session_get_status',
    ])
  })

  it('does not disconnect without a confirmed local session', async () => {
    invoke.mockResolvedValue({ state: 'disconnected', connected: false, expired: false })
    const { result } = renderHook(() => useAuth())
    await waitFor(() => expect(result.current.nativeStatus).toBe('disconnected'))
    await act(async () => { await result.current.signOut() })
    expect(invoke).toHaveBeenCalledTimes(1)
  })
})
