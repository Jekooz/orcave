// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { AuthPage } from './AuthPage'

const useAuth = vi.fn()
vi.mock('@/hooks/useAuth', () => ({ useAuth: () => useAuth() }))
const refreshStatus = vi.fn()
const signOut = vi.fn()

afterEach(cleanup)
beforeEach(() => {
  refreshStatus.mockReset()
  signOut.mockReset()
  useAuth.mockReturnValue({
    loading: false, nativeStatus: 'disconnected', actionError: false,
    refreshStatus, signOut, isAuthenticated: false,
  })
})

describe('AuthPage connection UI', () => {
  it('disables unimplemented connection and offers safe Comet installation guidance', () => {
    render(<AuthPage />)
    expect(screen.getByRole('button', { name: 'Connect to YouTube Music (not available)' }).hasAttribute('disabled')).toBe(true)
    expect(screen.getByText(/CometHTM companion installation and browser integration have not been verified/)).toBeTruthy()
    expect(screen.getByText(/Do not install extensions from unknown sources/)).toBeTruthy()
    expect(screen.queryByRole('textbox')).toBeNull()
    expect(screen.queryByText(/Connected to YouTube Music/)).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Check local session status' }))
    expect(refreshStatus).toHaveBeenCalledOnce()
  })

  it('labels native connected as unverified and disconnects only local session', () => {
    useAuth.mockReturnValue({ ...useAuth(), nativeStatus: 'local_session' })
    render(<AuthPage />)
    expect(screen.getByText('Local native session detected. YouTube Music access is not verified.')).toBeTruthy()
    expect(screen.queryByText('Connected to YouTube Music')).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Disconnect local session' }))
    expect(signOut).toHaveBeenCalledOnce()
  })

  it('shows expired and failed status without exposing native errors', () => {
    useAuth.mockReturnValue({ ...useAuth(), nativeStatus: 'expired', actionError: true })
    render(<AuthPage />)
    expect(screen.getByText(/Local native session expired/)).toBeTruthy()
    expect(screen.getByRole('alert').textContent).toContain('Native session action failed. Try again.')
    expect(screen.queryByRole('button', { name: 'Disconnect local session' })).toBeNull()
  })
})
