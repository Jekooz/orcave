import { useDiagnostics } from '@/hooks/useDiagnostics'
import { Filter, Download, Trash2, AlertCircle, AlertTriangle, Info, CheckCircle } from 'lucide-react'
import { useState, useMemo, useRef, useEffect } from 'react'
import { clsx } from 'clsx'
import { invoke } from '@/lib/tauri'

interface NativeProbe {
  pid: number
  platform: string
  version: string
}

// Fields that must never leave the diagnostic export.
const SECRET_FIELD_KEYS = [
  'token', 'access_token', 'refresh_token', 'id_token', 'code', 'state',
  'credential', 'password', 'secret', 'api_key', 'apikey', 'auth',
  'session', 'cookie', 'authorization',
]
// URL fields that carry signed media URLs and must be redacted.
const SIGNED_URL_KEYS = ['stream_url', 'playback_url', 'url']

function isSecretKey(key: string): boolean {
  const lower = key.toLowerCase()
  return SECRET_FIELD_KEYS.some((k) => lower.includes(k)) || SIGNED_URL_KEYS.includes(lower)
}

function redactValue(key: string, value: unknown): unknown {
  if (isSecretKey(key)) {
    if (typeof value === 'string' && value.length > 0) {
      return '[REDACTED]'
    }
    return '[REDACTED]'
  }
  if (typeof value === 'string' && SIGNED_URL_KEYS.includes(key.toLowerCase()) && value.startsWith('http')) {
    return '[REDACTED_SIGNED_URL]'
  }
  return value
}

function redactObject(obj: unknown): unknown {
  if (obj === null || obj === undefined) return obj
  if (Array.isArray(obj)) return obj.map(redactObject)
  if (typeof obj === 'object') {
    const result: Record<string, unknown> = {}
    for (const [key, value] of Object.entries(obj as Record<string, unknown>)) {
      result[key] = redactValue(key, value)
      if (result[key] === value && typeof value === 'object' && value !== null) {
        result[key] = redactObject(value)
      }
    }
    return result
  }
  return obj
}

export function DiagnosticsPage() {
  const { events, streamErrors, streamErrorCount, maxEvents, clear, setMax,
    getEventsBySeverity, listening, subscriptionError } = useDiagnostics()
  const [filterType, setFilterType] = useState<'all' | 'error' | 'warning' | 'info' | 'debug'>('all')
  const [filterSource, setFilterSource] = useState<string>('all')
  const [autoScroll, setAutoScroll] = useState(true)
  const [nativeProbe, setNativeProbe] = useState<NativeProbe | null>(null)
  const [probeError, setProbeError] = useState('')
  const [probeStatus, setProbeStatus] = useState<'idle' | 'probing' | 'ok' | 'failed'>('idle')
  const [exportStatus, setExportStatus] = useState<'idle' | 'ok' | 'failed'>('idle')
  const logRef = useRef<HTMLDivElement>(null)

  const runNativeProbe = async () => {
    setNativeProbe(null)
    setProbeError('')
    setProbeStatus('probing')
    try {
      const result = await invoke<NativeProbe>('diagnostics_native_probe')
      setNativeProbe(result)
      setProbeStatus('ok')
    } catch (error) {
      setProbeError(`Native probe failed: ${String(error)}`)
      setProbeStatus('failed')
    }
  }

  // Auto-scroll to bottom when new events arrive
  useEffect(() => {
    if (autoScroll && logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight
    }
  }, [events.length, streamErrors.length, autoScroll])

  const filteredEvents = useMemo(() => {
    let result = events

    if (filterType !== 'all') {
      result = result.filter((e) => e.data.severity === filterType)
    }

    if (filterSource !== 'all') {
      result = result.filter((e) => e.type === filterSource)
    }

    return result
  }, [events, filterType, filterSource])

  const sources = useMemo(() => {
    const set = new Set<string>()
    events.forEach((e) => set.add(e.type))
    streamErrors.forEach(() => set.add('catalogue_stream_error'))
    return Array.from(set).sort()
  }, [events, streamErrors])

  const exportLogs = () => {
    setExportStatus('idle')
    try {
      const safe = {
        events: events.map((e) => ({
          type: e.type,
          data: redactObject(e.data),
          timestamp: e.timestamp,
        })),
        stream_errors: streamErrors.map((e) => ({
          video_id: e.video_id,
          error_code: e.error_code,
          message: e.message,
          retryable: e.retryable,
          timestamp: e.timestamp,
        })),
        exported_at: new Date().toISOString(),
      }
      const data = JSON.stringify(safe, null, 2)
      const blob = new Blob([data], { type: 'application/json' })
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = `orcave-diagnostics-${new Date().toISOString()}.json`
      a.click()
      URL.revokeObjectURL(url)
      setExportStatus('ok')
      setTimeout(() => setExportStatus('idle'), 2000)
    } catch (error) {
      setExportStatus('failed')
      setTimeout(() => setExportStatus('idle'), 2000)
    }
  }

  const getSeverityIcon = (severity?: string) => {
    switch (severity) {
      case 'error': return <AlertCircle className="w-4 h-4 text-orcave-error" />
      case 'warning': return <AlertTriangle className="w-4 h-4 text-orcave-warning" />
      case 'info': return <Info className="w-4 h-4 text-orcave-primary" />
      case 'debug': return <CheckCircle className="w-4 h-4 text-orcave-muted" />
      default: return <Info className="w-4 h-4 text-orcave-muted" />
    }
  }

  const formatTimestamp = (ts: string) => {
    const date = new Date(ts)
    return `${date.toLocaleTimeString('en-US', { hour12: false })}.${date.getMilliseconds().toString().padStart(3, '0')}`
  }

  const totalErrors = getEventsBySeverity('error').length + streamErrors.length

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
        <div>
          <h1 className="text-2xl font-bold">Diagnostics</h1>
          <p className="text-orcave-muted">Real-time system diagnostics and debug logs</p>
        </div>

        <div className="flex items-center gap-3">
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={autoScroll}
              onChange={(e) => setAutoScroll(e.target.checked)}
              className="w-4 h-4 rounded border-orcave-border bg-orcave-surface accent-orcave-primary"
            />
            Auto-scroll
          </label>

          <button onClick={exportLogs} className="btn-secondary" disabled={events.length === 0 && streamErrors.length === 0}>
            <Download className="w-4 h-4 mr-1" />
            {exportStatus === 'ok' ? 'Exported' : exportStatus === 'failed' ? 'Export failed' : 'Export'}
          </button>

          <button onClick={clear} className="btn-secondary text-orcave-error hover:bg-orcave-error/10" disabled={events.length === 0 && streamErrors.length === 0}>
            <Trash2 className="w-4 h-4 mr-1" />
            Clear
          </button>
        </div>
      </div>

      {/* Native process probe */}
      <section className="card p-4" aria-label="Native process probe">
        <button type="button" onClick={runNativeProbe} disabled={probeStatus === 'probing'} className="btn-secondary">
          {probeStatus === 'probing' ? (
            <>
              <svg className="animate-spin -ml-1 mr-2 h-4 w-4" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24"><circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4"></circle><path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path></svg>
              Probing native process...
            </>
          ) : 'Probe native process'}
        </button>
        <div role="status" className="mt-2 text-sm">
          {probeError && (
            <p className="text-orcave-error">{probeError}</p>
          )}
          {probeStatus === 'ok' && (
            <p className="text-orcave-success">Probe complete.</p>
          )}
          {nativeProbe && (
            <p>Native PID: {nativeProbe.pid} · Platform: {nativeProbe.platform} · Version: {nativeProbe.version}</p>
          )}
        </div>
      </section>

      {/* Stats Bar */}
      <div className="card p-4">
        <div className="grid grid-cols-2 sm:grid-cols-4 gap-4">
          <div className="text-center">
            <p className="text-2xl font-bold text-orcave-text">{events.length}</p>
            <p className="text-xs text-orcave-muted">Diagnostic Events</p>
          </div>
          <div className="text-center">
            <p className="text-2xl font-bold text-orcave-error">{totalErrors}</p>
            <p className="text-xs text-orcave-muted">Errors (incl. stream)</p>
          </div>
          <div className="text-center">
            <p className="text-2xl font-bold text-orcave-warning">{getEventsBySeverity('warning').length}</p>
            <p className="text-xs text-orcave-muted">Warnings</p>
          </div>
          <div className="text-center">
            <p className="text-2xl font-bold text-orcave-primary">{getEventsBySeverity('info').length}</p>
            <p className="text-xs text-orcave-muted">Info</p>
          </div>
        </div>
      </div>

      {/* Filters */}
      <div className="card p-4">
        <div className="flex flex-wrap gap-4">
          <div className="flex items-center gap-2">
            <Filter className="w-4 h-4 text-orcave-muted" />
            <select aria-label="Severity"
              value={filterType}
              onChange={(e) => setFilterType(e.target.value as typeof filterType)}
              className="input w-32"
            >
              <option value="all">All</option>
              <option value="error">Errors</option>
              <option value="warning">Warnings</option>
              <option value="info">Info</option>
              <option value="debug">Debug</option>
            </select>
          </div>

          <div className="flex items-center gap-2">
            <select aria-label="Source"
              value={filterSource}
              onChange={(e) => setFilterSource(e.target.value)}
              className="input w-48"
            >
              <option value="all">All Sources</option>
              {sources.map((source) => (
                <option key={source} value={source}>{source}</option>
              ))}
            </select>
          </div>

          <div className="flex items-center gap-2">
            <label htmlFor="max-events" className="text-sm text-orcave-muted">Max Events:</label>
            <select
              id="max-events"
              value={maxEvents}
              onChange={(e) => setMax(parseInt(e.target.value))}
              className="input w-24"
            >
              <option value={100}>100</option>
              <option value={500}>500</option>
              <option value={1000}>1000</option>
              <option value={5000}>5000</option>
            </select>
          </div>
        </div>
      </div>

      {/* Stream errors */}
      {streamErrors.length > 0 && (
        <section className="card p-4" aria-label="Stream errors">
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-lg font-semibold text-orcave-text">Stream Errors</h2>
            <span className="badge badge-error">{streamErrors.length} retryable={streamErrors.filter(e => e.retryable).length}</span>
          </div>
          <div className="space-y-1 p-4 font-mono text-xs max-h-64 overflow-y-auto">
            {streamErrors.map((error, index) => (
              <div key={index} className="diagnostic-entry flex items-start gap-3 diagnostic-entry-error">
                <span className="text-orcave-muted/50 w-24 shrink-0">{formatTimestamp(error.timestamp)}</span>
                <span className="badge badge-error shrink-0">catalogue_stream_error</span>
                <AlertCircle className="w-4 h-4 text-orcave-error" />
                <span className="flex-1 min-w-0 break-all text-orcave-text/90">
                  {error.video_id} · {error.error_code} · {error.message} · retryable={String(error.retryable)}
                </span>
              </div>
            ))}
          </div>
        </section>
      )}

      {/* Log View */}
      <div className="card diagnostic-log relative" ref={logRef}>
        {filteredEvents.length === 0 && streamErrors.length === 0 ? (
          <div className="flex flex-col items-center justify-center h-full text-orcave-muted">
            <p>No diagnostic events yet</p>
            <p className="text-sm">Events will appear here as the system runs</p>
          </div>
        ) : (
          <div className="space-y-1 p-4">
            {filteredEvents.map((event, index) => (
              <div
                key={index}
                className={clsx(
                  'diagnostic-entry flex items-start gap-3 font-mono text-xs',
                  event.data.severity === 'error' && 'diagnostic-entry-error',
                  event.data.severity === 'warning' && 'diagnostic-entry-warning',
                  event.data.severity === 'info' && 'diagnostic-entry-info',
                  event.data.severity === 'debug' && 'opacity-60'
                )}>
                <span className="text-orcave-muted/50 w-24 shrink-0">{formatTimestamp(event.timestamp)}</span>
                <span className="badge badge-primary shrink-0">{event.type}</span>
                {getSeverityIcon(typeof event.data.severity === 'string' ? event.data.severity : undefined)}
                <span className="flex-1 min-w-0 break-all text-orcave-text/90">
                  {typeof event.data.message === 'string' ? event.data.message : JSON.stringify(event.data)}
                </span>
              </div>
            ))}
          </div>
        )}

        {/* Bottom indicator */}
        <div className="absolute bottom-0 left-0 right-0 h-8 bg-gradient-to-t from-orcave-bg to-transparent pointer-events-none" />
      </div>

      {/* Live Indicator */}
      <div className="card p-4">
        <div className="flex items-center gap-3">
           <div className={clsx('w-3 h-3 rounded-full', listening ? 'bg-orcave-success' : 'bg-orcave-warning')} />
           <span className="text-sm font-medium" role="status">{subscriptionError || (listening ? 'Live monitoring active' : 'Connecting to native diagnostics...')}</span>
           <span className="text-orcave-muted ml-auto">
             Source: app_diagnostic · catalogue_stream_error · {streamErrorCount} stream errors
           </span>
        </div>
      </div>
    </div>
  )
}
