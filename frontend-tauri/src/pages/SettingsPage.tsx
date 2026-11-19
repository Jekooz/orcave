import { useSettings } from '@/hooks/useSettings'
import { useAuth } from '@/hooks/useAuth'
import { SectionTitle } from '@/components/SectionTitle'
import { Music, Volume2, Monitor, Shield, Database, Trash2, Download, Upload, AlertCircle } from 'lucide-react'
import { useState, useEffect, useCallback } from 'react'
import { clsx } from 'clsx'
import { Link } from 'react-router-dom'
import { invoke } from '@/lib/tauri'
import type { Settings, AudioOutputDevice } from '@/lib/tauri'

export function SettingsPage() {
  const { settings, loading, update, reset } = useSettings()
  const { isAuthenticated, signOut } = useAuth()
  const [activeSection, setActiveSection] = useState<string>('account')

  const sections = [
    { id: 'account', label: 'Account', icon: Shield },
    { id: 'audio', label: 'Audio', icon: Music },
    { id: 'playback', label: 'Playback', icon: Volume2 },
    { id: 'appearance', label: 'Appearance', icon: Monitor },
    { id: 'network', label: 'Network', icon: Database },
    { id: 'cache', label: 'Cache', icon: Download },
    { id: 'diagnostics', label: 'Diagnostics', icon: Upload },
    { id: 'advanced', label: 'Advanced', icon: Shield },
  ]

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <Loader2 className="w-8 h-8 text-orcave-primary animate-spin" />
      </div>
    )
  }
  if (!settings) return <p role="status">Settings unavailable. Restart the app and try again.</p>

  return (
    <div className="max-w-4xl mx-auto space-y-6">
      <div className="flex gap-6">
        {/* Sidebar Navigation */}
        <aside className="w-48 flex-shrink-0">
          <nav className="space-y-1">
            {sections.map((section) => {
              const Icon = section.icon
              return (
                <button
                  key={section.id}
                  onClick={() => setActiveSection(section.id)}
                  className={clsx(
                    'w-full flex items-center gap-3 px-3 py-2 rounded-lg text-left text-sm transition-colors',
                    activeSection === section.id
                      ? 'bg-orcave-primary text-orcave-bg'
                      : 'text-orcave-muted hover:text-orcave-text hover:bg-orcave-surfaceElevated'
                  )}
                >
                  <Icon className="w-5 h-5 flex-shrink-0" />
                  {section.label}
                </button>
              )
            })}
          </nav>
        </aside>

        {/* Content */}
        <div className="flex-1">
          {activeSection === 'account' && (
            <AccountSettings settings={settings} update={update} isAuthenticated={isAuthenticated} signOut={signOut} />
          )}
          {activeSection === 'audio' && <AudioSettings settings={settings} update={update} />}
          {activeSection === 'playback' && <PlaybackSettings settings={settings} update={update} />}
          {activeSection === 'appearance' && <AppearanceSettings settings={settings} update={update} />}
          {activeSection === 'network' && <NetworkSettings settings={settings} update={update} />}
          {activeSection === 'cache' && <CacheSettings settings={settings} update={update} />}
          {activeSection === 'diagnostics' && <DiagnosticsSettings settings={settings} update={update} />}
          {activeSection === 'advanced' && <AdvancedSettings settings={settings} update={update} reset={reset} />}
        </div>
      </div>
    </div>
  )
}

// Section Components
type UpdateSetting = <K extends keyof Settings>(key: K, value: Settings[K]) => Promise<void>
type SectionProps = { settings: Settings; update: UpdateSetting }

function AccountSettings({ settings, update, isAuthenticated, signOut }: SectionProps & { isAuthenticated: boolean; signOut: () => Promise<void> }) {
  return (
    <div className="space-y-6">
      <SectionTitle>Account</SectionTitle>

      <div className="card p-6 space-y-4">
        {isAuthenticated ? (
          <>
            <div className="flex items-center gap-4">
              <div className="w-16 h-16 rounded-full bg-orcave-primary flex items-center justify-center text-orcave-bg font-bold text-2xl">
                {settings?.auth_auto_sign_in ? '✓' : '?'}
              </div>
              <div>
                <h3 className="font-semibold">Signed in</h3>
                <p className="text-orcave-muted text-sm">YouTube Music account connected</p>
              </div>
            </div>
            <div className="pt-4 border-t border-orcave-border space-y-3">
              <label className="flex items-center justify-between">
                <div>
                  <p className="font-medium">Auto sign-in</p>
                  <p className="text-sm text-orcave-muted">Automatically sign in on app start</p>
                </div>
                <Switch checked={settings?.auth_auto_sign_in} onChange={(v) => update('auth_auto_sign_in', v)} />
              </label>
              <label className="flex items-center justify-between">
                <div>
                  <p className="font-medium">Remember credentials</p>
                  <p className="text-sm text-orcave-muted">Store encrypted credentials for faster sign-in</p>
                </div>
                <Switch checked={settings?.auth_remember_credentials} onChange={(v) => update('auth_remember_credentials', v)} />
              </label>
              <button onClick={signOut} className="btn-danger w-full">Sign Out</button>
            </div>
          </>
        ) : (
          <div className="text-center py-8">
            <p className="text-orcave-muted mb-4">Not signed in</p>
            <Link to="/auth" className="btn-primary">Go to Sign In</Link>
          </div>
        )}
      </div>
    </div>
  )
}

function AudioSettings({ settings, update }: SectionProps) {
  return (
    <div className="space-y-6">
      <SectionTitle>Audio Quality</SectionTitle>

      <div className="card p-6 space-y-6">
        <div>
          <label className="block text-sm font-medium mb-2">Streaming Quality</label>
          <Select
            value={settings?.audio_quality || 'high'}
            onChange={(v) => update('audio_quality', v)}
            options={[
              { value: 'low', label: 'Low (48 kbps AAC)' },
              { value: 'normal', label: 'Normal (128 kbps AAC)' },
              { value: 'high', label: 'High (256 kbps AAC)' },
              { value: 'lossless', label: 'Lossless (FLAC when available)' },
            ]}
          />
        </div>

        <div>
          <label className="block text-sm font-medium mb-2">Volume Normalization</label>
          <Switch checked={settings?.audio_normalize_volume} onChange={(v) => update('audio_normalize_volume', v)} />
          <p className="text-sm text-orcave-muted mt-1">Normalize volume to -14 LUFS (EBU R128)</p>
        </div>

        <div className="pt-4 border-t border-orcave-border">
          <h4 className="font-medium mb-4">Crossfade</h4>
          <div className="space-y-4">
            <label className="flex items-center justify-between">
              <div>
                <p className="font-medium">Enable Crossfade</p>
                <p className="text-sm text-orcave-muted">Smooth transitions between tracks</p>
              </div>
              <Switch checked={settings?.audio_crossfade?.enabled} onChange={(v) => update('audio_crossfade', { ...settings.audio_crossfade, enabled: v })} />
            </label>
            {settings?.audio_crossfade?.enabled && (
              <div>
                <label className="block text-sm font-medium mb-2">Duration: {settings?.audio_crossfade?.duration_ms}ms</label>
                <Slider
                  value={settings?.audio_crossfade?.duration_ms || 5000}
                  min={1000}
                  max={10000}
                  step={500}
                  onChange={(v) => update('audio_crossfade', { ...settings.audio_crossfade, duration_ms: v })}
                />
              </div>
            )}
          </div>
        </div>

        <div className="pt-4 border-t border-orcave-border">
          <h4 className="font-medium mb-4">Gapless Playback</h4>
          <div className="space-y-4">
            <label className="flex items-center justify-between">
              <div>
                <p className="font-medium">Enable Gapless</p>
                <p className="text-sm text-orcave-muted">Seamless playback between tracks</p>
              </div>
              <Switch checked={settings?.audio_gapless?.enabled} onChange={(v) => update('audio_gapless', { ...settings.audio_gapless, enabled: v })} />
            </label>
            <label className="flex items-center justify-between">
              <div>
                <p className="font-medium">Preload Next Track</p>
                <p className="text-sm text-orcave-muted">Prepare next track for instant playback</p>
              </div>
              <Switch checked={settings?.audio_gapless?.preload_next} onChange={(v) => update('audio_gapless', { ...settings.audio_gapless, preload_next: v })} />
            </label>
          </div>
        </div>

        <div className="pt-4 border-t border-orcave-border">
          <h4 className="font-medium mb-4">Output Device</h4>
          <OutputDeviceSettings settings={settings} update={update} />
        </div>
      </div>
    </div>
  )
}

function OutputDeviceSettings({ settings, update }: SectionProps) {
  const [devices, setDevices] = useState<AudioOutputDevice[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [selected, setSelected] = useState(settings?.audio_output_device ?? '')

  const loadDevices = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const list = await invoke<AudioOutputDevice[]>('audio_get_output_devices')
      setDevices(list)
    } catch (reason) {
      setError(String(reason))
    } finally {
      setLoading(false)
    }
  }, [])

  const applyDevice = useCallback(async () => {
    setError(null)
    try {
      await invoke('audio_set_output_device', { deviceId: selected })
      await update('audio_output_device', selected || undefined)
    } catch (reason) {
      setError(String(reason))
    }
  }, [selected, update])

  useEffect(() => {
    void loadDevices()
  }, [loadDevices])

  const activeDevice = devices.find((d) => d.is_active)
  const isApplied = !!selected && devices.some((d) => d.id === selected && d.is_active)

  return (
    <div className="space-y-4">
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-3">
        <label htmlFor="output-device-select" className="text-sm font-medium">Output device</label>
        <select
          id="output-device-select"
          value={selected}
          onChange={(e) => setSelected(e.target.value)}
          disabled={loading || devices.length === 0}
          className="input flex-1"
        >
          <option value="">System default</option>
          {devices.map((device) => (
            <option key={device.id} value={device.id}>
              {device.name}{device.is_active ? ' (active)' : ''}{device.is_default ? ' (default)' : ''}
            </option>
          ))}
        </select>
        <div className="flex items-center gap-2">
          <button onClick={() => void loadDevices()} disabled={loading} className="btn-secondary">
            Refresh
          </button>
          <button
            onClick={() => void applyDevice()}
            disabled={!selected || loading || isApplied}
            className="btn-primary"
          >
            Apply
          </button>
        </div>
      </div>

      {activeDevice && !selected && (
        <p className="text-sm text-orcave-muted">Active output: {activeDevice.name}</p>
      )}

      {error && (
        <p role="alert" className="p-3 rounded-lg bg-orcave-error/10 border border-orcave-error/30 text-orcave-error text-sm flex items-center gap-2">
          <AlertCircle className="w-4 h-4 shrink-0" aria-hidden="true" />
          Output device error: {error}
        </p>
      )}

      {!error && devices.length === 0 && !loading && (
        <p className="text-sm text-orcave-muted">No output devices available. Device selection is provided by the audio backend.</p>
      )}
    </div>
  )
}

function PlaybackSettings({ settings, update }: SectionProps) {
  return (
    <div className="space-y-6">
      <SectionTitle>Playback</SectionTitle>

      <div className="card p-6 space-y-6">
        <div>
          <label className="block text-sm font-medium mb-2">Repeat Mode</label>
          <Select
            value={settings?.playback_repeat_mode || 'off'}
            onChange={(v) => update('playback_repeat_mode', v)}
            options={[
              { value: 'off', label: 'Off' },
              { value: 'all', label: 'Repeat All' },
              { value: 'one', label: 'Repeat One' },
            ]}
          />
        </div>

        <label className="flex items-center justify-between">
          <div>
            <p className="font-medium">Shuffle</p>
            <p className="text-sm text-orcave-muted">Randomize playback order</p>
          </div>
          <Switch checked={settings?.playback_shuffle} onChange={(v) => update('playback_shuffle', v)} />
        </label>
      </div>
    </div>
  )
}

function AppearanceSettings({ settings, update }: SectionProps) {
  return (
    <div className="space-y-6">
      <SectionTitle>Appearance</SectionTitle>

      <div className="card p-6 space-y-6">
        <div>
          <label className="block text-sm font-medium mb-2">Theme</label>
          <Select
            value={settings?.theme || 'system'}
            onChange={(v) => update('theme', v)}
            options={[
              { value: 'system', label: 'System' },
              { value: 'light', label: 'Light' },
              { value: 'dark', label: 'Dark' },
            ]}
          />
        </div>

        <div className="grid grid-cols-2 gap-4">
          <label className="flex items-center justify-between">
            <div>
              <p className="font-medium">Compact Mode</p>
              <p className="text-sm text-orcave-muted">Reduce spacing for more content</p>
            </div>
            <Switch checked={settings?.ui_compact_mode} onChange={(v) => update('ui_compact_mode', v)} />
          </label>

          <label className="flex items-center justify-between">
            <div>
              <p className="font-medium">Animations</p>
              <p className="text-sm text-orcave-muted">Enable UI transitions</p>
            </div>
            <Switch checked={settings?.ui_animations} onChange={(v) => update('ui_animations', v)} />
          </label>
        </div>

        <div>
          <label className="block text-sm font-medium mb-2">Language</label>
          <Select
            value={settings?.ui_language || 'en-US'}
            onChange={(v) => update('ui_language', v)}
            options={[
              { value: 'en-US', label: 'English (US)' },
              { value: 'en-GB', label: 'English (UK)' },
              { value: 'de-DE', label: 'German' },
              { value: 'es-ES', label: 'Spanish' },
              { value: 'fr-FR', label: 'French' },
              { value: 'ja-JP', label: 'Japanese' },
              { value: 'ko-KR', label: 'Korean' },
              { value: 'zh-CN', label: 'Chinese (Simplified)' },
            ]}
          />
        </div>
      </div>
    </div>
  )
}

function NetworkSettings({ settings, update }: SectionProps) {
  return (
    <div className="space-y-6">
      <SectionTitle>Network</SectionTitle>

      <div className="card p-6 space-y-6">
        <div>
          <label className="block text-sm font-medium mb-2">Proxy (optional)</label>
          <input
            type="text"
            placeholder="http://proxy:port"
            value={settings?.network_proxy || ''}
            onChange={(e) => update('network_proxy', e.target.value)}
            className="input"
          />
        </div>

        <div>
          <label className="block text-sm font-medium mb-2">Request Timeout: {settings?.network_timeout_secs}s</label>
          <Slider
            value={settings?.network_timeout_secs || 30}
            min={5}
            max={120}
            step={5}
            onChange={(v) => update('network_timeout_secs', v)}
          />
        </div>
      </div>
    </div>
  )
}

function CacheSettings({ settings, update }: SectionProps) {
  return (
    <div className="space-y-6">
      <SectionTitle>Cache & Storage</SectionTitle>

      <div className="card p-6 space-y-6">
        <div>
          <label className="block text-sm font-medium mb-2">Max Cache Size: {settings?.cache_max_size_mb} MB</label>
          <Slider
            value={settings?.cache_max_size_mb || 500}
            min={100}
            max={5000}
            step={100}
            onChange={(v) => update('cache_max_size_mb', v)}
          />
        </div>

        <div className="grid grid-cols-2 gap-4">
          <label className="flex items-center justify-between">
            <div>
              <p className="font-medium">Cache Artwork</p>
              <p className="text-sm text-orcave-muted">Store album/track images locally</p>
            </div>
            <Switch checked={settings?.cache_artwork} onChange={(v) => update('cache_artwork', v)} />
          </label>

          <label className="flex items-center justify-between">
            <div>
              <p className="font-medium">Preload Audio</p>
              <p className="text-sm text-orcave-muted">Cache next track for gapless playback</p>
            </div>
            <Switch checked={settings?.cache_audio_preload} onChange={(v) => update('cache_audio_preload', v)} />
          </label>
        </div>

      </div>
    </div>
  )
}

function DiagnosticsSettings({ settings, update }: SectionProps) {
  return (
    <div className="space-y-6">
      <SectionTitle>Diagnostics</SectionTitle>

      <div className="card p-6 space-y-6">
        <label className="flex items-center justify-between">
          <div>
            <p className="font-medium">Enable Diagnostics</p>
            <p className="text-sm text-orcave-muted">Collect performance and debug data</p>
          </div>
          <Switch checked={settings?.diagnostics_enabled} onChange={(v) => update('diagnostics_enabled', v)} />
        </label>

        <div>
          <label className="block text-sm font-medium mb-2">Log Level</label>
          <Select
            value={settings?.diagnostics_log_level || 'info'}
            onChange={(v) => update('diagnostics_log_level', v)}
            options={[
              { value: 'trace', label: 'Trace' },
              { value: 'debug', label: 'Debug' },
              { value: 'info', label: 'Info' },
              { value: 'warn', label: 'Warning' },
              { value: 'error', label: 'Error' },
            ]}
          />
        </div>

        <label className="flex items-center justify-between">
          <div>
            <p className="font-medium">Crash Reporting</p>
            <p className="text-sm text-orcave-muted">Send anonymous crash reports</p>
          </div>
          <Switch checked={settings?.diagnostics_crash_reporting} onChange={(v) => update('diagnostics_crash_reporting', v)} />
        </label>
      </div>
    </div>
  )
}

function AdvancedSettings({ settings, update, reset }: SectionProps & { reset: () => Promise<void> }) {
  return (
    <div className="space-y-6">
      <SectionTitle>Advanced</SectionTitle>

      <div className="card p-6 space-y-6">
        <label className="flex items-center justify-between">
          <div>
            <p className="font-medium">Experimental Features</p>
            <p className="text-sm text-orcave-muted">Enable unstable features (may cause issues)</p>
          </div>
          <Switch checked={settings?.advanced_experimental_features} onChange={(v) => update('advanced_experimental_features', v)} />
        </label>

        <label className="flex items-center justify-between">
          <div>
            <p className="font-medium">Developer Mode</p>
            <p className="text-sm text-orcave-muted">Show debug menus and detailed errors</p>
          </div>
          <Switch checked={settings?.advanced_developer_mode} onChange={(v) => update('advanced_developer_mode', v)} />
        </label>

        <div className="pt-4 border-t border-orcave-border">
          <h4 className="font-medium mb-3">Danger Zone</h4>
           <button onClick={() => { if (window.confirm('Reset all settings to defaults?')) reset().catch(console.error) }} className="btn-danger">
            <Trash2 className="w-4 h-4 mr-2" />
            Reset All Settings to Defaults
          </button>
          <p className="text-sm text-orcave-muted">This cannot be undone.</p>
        </div>
      </div>
    </div>
  )
}

import { Loader2 } from 'lucide-react'

// UI Components
function Switch({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <button
      type="button"
      role="switch"
      onClick={() => onChange(!checked)}
      className={clsx(
        'relative w-11 h-6 rounded-full transition-colors',
        checked ? 'bg-orcave-primary' : 'bg-orcave-border'
      )}
      aria-checked={checked}
    >
      <span
        className={clsx(
          'absolute top-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform',
          checked ? 'translate-x-5' : 'translate-x-0.5'
        )}
      />
    </button>
  )
}

function Select<T extends string>({ value, onChange, options }: { value: T; onChange: (v: T) => void; options: { value: T; label: string }[] }) {
  return (
    <select
      value={value}
      onChange={(e) => {
        const option = options.find((entry) => entry.value === e.target.value)
        if (option) onChange(option.value)
      }}
      aria-label={options.length ? 'Setting' : undefined}
      className="input"
    >
      {options.map((opt) => (
        <option key={opt.value} value={opt.value}>{opt.label}</option>
      ))}
    </select>
  )
}

function Slider({ value, min, max, step, onChange }: { value: number; min: number; max: number; step: number; onChange: (v: number) => void }) {
  return (
    <input
      type="range"
      min={min}
      max={max}
      step={step}
      value={value}
      onChange={(e) => onChange(parseFloat(e.target.value))}
      className="w-full h-2 appearance-none bg-orcave-border rounded-full accent-orcave-primary"
    />
  )
}
