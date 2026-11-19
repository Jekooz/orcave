import { useCallback, useEffect, useState } from 'react'
import { listen } from '@/lib/tauri'
import { useDiagnosticsStore } from '@/lib/stores'

export function useDiagnostics() {
  const { events, streamErrors, maxEvents, addEvent, addStreamError, clearEvents, setMaxEvents } = useDiagnosticsStore()
  const [subscriptionError, setSubscriptionError] = useState('')
  const [listening, setListening] = useState(false)
  const [streamErrorCount, setStreamErrorCount] = useState(0)

  // App diagnostics arrive without a timestamp; stamp and redact them on receipt.
  useEffect(() => {
    let cancelled = false
    let unsubscribeApp: (() => void) | undefined
    let unsubscribeStream: (() => void) | undefined
    setListening(false)
    setSubscriptionError('')

    // app_diagnostic: redacted diagnostic events from native.
    listen<{ type: string; data: Record<string, unknown> }>('app_diagnostic', ({ payload }) => {
      if (cancelled || !payload || typeof payload.type !== 'string') return
      const data = payload.data ?? {}
      // Only known, non-secret fields are allowed into the log and export.
      // 'severity' is always allowed so error/warning/info filtering works.
      const fields: Record<string, string[]> = {
        AuthFlowStarted: ['provider'],
        AuthFlowCompleted: ['success', 'duration_ms'],
        CatalogueRequest: ['latency_ms', 'cached'],
        AudioPipelineEvent: ['stage'],
        QueueOperation: ['operation', 'item_count'],
        RecommendationRequest: ['seed_type', 'count', 'latency_ms'],
        SettingsChanged: ['key'],
        WindowsApiCall: ['api', 'success'],
        PerformanceMetric: ['name', 'value', 'unit'],
      }
      const allowed = new Set([...(fields[payload.type] ?? []), 'severity'])
      const safeData: Record<string, unknown> = {}
      for (const key of allowed) {
        const value = data[key]
        if (typeof value === 'boolean' || typeof value === 'number' || typeof value === 'string') {
          safeData[key] = value
        }
      }
      addEvent({ type: payload.type, data: safeData, timestamp: new Date().toISOString() })
    }).then((stop) => {
      if (cancelled) stop()
      else { unsubscribeApp = stop }
    }).catch((error) => {
      if (!cancelled) setSubscriptionError(`Diagnostic listener failed: ${String(error)}`)
    })

    // catalogue_stream_error: buffering/retry status from catalogue subsystem.
    listen<{ video_id: string; error_code: string; message: string; retryable: boolean }>('catalogue_stream_error', ({ payload }) => {
      if (cancelled) return
      addStreamError({
        video_id: payload.video_id,
        error_code: payload.error_code,
        message: payload.message,
        retryable: payload.retryable,
        timestamp: new Date().toISOString(),
      })
      setStreamErrorCount((n) => n + 1)
    }).then((stop) => {
      if (cancelled) stop()
      else { unsubscribeStream = stop }
    }).catch((error) => {
      if (!cancelled) setSubscriptionError(`${subscriptionError} Stream listener failed: ${String(error)}`)
    })

    setListening(true)
    return () => {
      cancelled = true
      unsubscribeApp?.()
      unsubscribeStream?.()
    }
  }, [addEvent, addStreamError])

  const clear = useCallback(() => {
    clearEvents()
    setStreamErrorCount(0)
  }, [clearEvents])

  const setMax = useCallback((max: number) => {
    setMaxEvents(max)
  }, [setMaxEvents])

  // Filter helpers
  const getEventsByType = useCallback((type: string) => {
    return events.filter((e) => e.type === type)
  }, [events])

  const getEventsBySeverity = useCallback((severity: 'error' | 'warning' | 'info' | 'debug') => {
    return events.filter((e) => e.data.severity === severity)
  }, [events])

  const getRecentErrors = useCallback(() => {
    return events.filter((e) => e.data.severity === 'error').slice(0, 50)
  }, [events])

  return {
    events,
    streamErrors,
    streamErrorCount,
    listening,
    subscriptionError,
    maxEvents,
    clear,
    setMax: setMax,
    getEventsByType,
    getEventsBySeverity,
    getRecentErrors,
  }
}
