import { useCallback, useEffect, useState } from 'react'
import { invoke } from '@/lib/tauri'
import { useAuthStore } from '@/lib/stores'
import type { NativeSessionStatus } from '@/lib/tauri'

type NativeStatus = 'unknown' | 'local_session' | 'disconnected' | 'expired' | 'error'

// Only native_session_get_status can describe local session material. Even a
// connected native session does not establish verified provider access.
function classifyStatus(value: NativeSessionStatus): NativeStatus {
  if (!value || typeof value !== 'object') return 'error'
  // Rust retains legacy native material as unverified; allow removal without
  // treating its absent handle/TTL as a provider authentication failure.
  if (value.state === 'unverified' && value.connected === false && value.expired === false)
    return 'local_session'
  if (value.state === 'connected' && value.connected === true && value.expired === false
    && Number.isSafeInteger(value.ttl_secs) && value.ttl_secs > 0
    && typeof value.handle === 'string' && value.handle.length > 0)
    return 'local_session'
  if (value.state === 'expired' || value.expired === true) return 'expired'
  if (value.state === 'disconnected' && value.connected === false && value.expired === false)
    return 'disconnected'
  return 'error'
}

export function useAuth(initialize = true) {
  const { loading, setState, setUser, setLoading } = useAuthStore()
  const [nativeStatus, setNativeStatus] = useState<NativeStatus>('unknown')
  const [actionError, setActionError] = useState(false)

  const refreshStatus = useCallback(async () => {
    try {
      const status = classifyStatus(await invoke<NativeSessionStatus>('native_session_get_status'))
      setNativeStatus(status)
      setState(status === 'expired' ? 'session_expired' : status === 'error' ? 'error' : 'unauthenticated')
      setUser(null)
      setActionError(false)
    } catch {
      setNativeStatus('error')
      setState('error')
      setUser(null)
      setActionError(true)
    }
  }, [setState, setUser])

  useEffect(() => {
    if (!initialize) return
    setLoading(true)
    void refreshStatus().finally(() => setLoading(false))
  }, [initialize, refreshStatus, setLoading])

  const signOut = useCallback(async () => {
    if (nativeStatus !== 'local_session') return
    try {
      await invoke('native_session_disconnect')
      await refreshStatus()
    } catch {
      setActionError(true)
    }
  }, [nativeStatus, refreshStatus])

  return {
    loading,
    nativeStatus,
    actionError,
    // Provider access is unverified until Rust exposes a separate validated
    // provider-session contract. This must not unlock protected routes.
    isAuthenticated: false,
    refreshStatus,
    signOut,
  }
}
