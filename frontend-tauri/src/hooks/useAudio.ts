import { useCallback, useEffect } from 'react'
import { invoke, listen } from '@/lib/tauri'
import { useAudioStore } from '@/lib/stores'
import type { TrackHandle, PlaybackState, AudioEvent } from '@/lib/tauri'

interface AudioErrorData {
  message?: string
  code?: string
}

// ---------------------------------------------------------------------------
// Temporary sanitized stage diagnostics (playback pipeline observation).
// Logging only. No raw media, file paths, stream URLs, tokens, credentials,
// or content hashes are logged. Track identifiers are shortened where shown.
// ---------------------------------------------------------------------------
interface DiagStage {
  started: number
  ended?: number
  success?: boolean
  errorCategory?: string
}

interface DiagCounters {
  playCalls: number
  playSuccess: number
  playFailure: number
  decodeFailures: number
  sinkFailures: number
  stateChangedEvents: number
  errorEvents: number
  positionEvents: number
}

const diagCounters: DiagCounters = {
  playCalls: 0,
  playSuccess: 0,
  playFailure: 0,
  decodeFailures: 0,
  sinkFailures: 0,
  stateChangedEvents: 0,
  errorEvents: 0,
  positionEvents: 0,
}

const diagStages = new Map<string, DiagStage>()

function diagNow(): number {
  return typeof performance !== 'undefined' && typeof performance.now === 'function'
    ? performance.now()
    : Date.now()
}

function diagStart(stage: string): void {
  diagStages.set(stage, { started: diagNow() })
}

function diagEnd(stage: string, success: boolean, errorCategory?: string): void {
  const t = diagStages.get(stage)
  if (!t) return
  t.ended = diagNow()
  t.success = success
  t.errorCategory = errorCategory
  const elapsed = t.ended - t.started
  console.info(
    `[audio-diag] stage=${stage} success=${success} elapsed_ms=${elapsed.toFixed(1)} error_category=${errorCategory ?? '-'} counters=${JSON.stringify(diagCounters)}`
  )
}

function diagCount(key: keyof DiagCounters, value = 1): void {
  diagCounters[key] += value
}

function shortId(id: string): string {
  return id.length > 8 ? `...${id.slice(-8)}` : id
}

export function useAudio() {
  const {
    state,
    currentTrack,
    position,
    duration,
    volume,
    error,
    setState,
    setCurrentTrack,
    setPosition,
    setDuration,
    setVolume,
    setError,
  } = useAudioStore()

  // Sink watchdog: detects "playing" state with no position advancement
  // (silent sink failure after the engine emits Playing).
  let positionStallTimer: ReturnType<typeof setTimeout> | null = null

  const clearStallWatchdog = (): void => {
    if (positionStallTimer) {
      clearTimeout(positionStallTimer)
      positionStallTimer = null
    }
  }

  const startStallWatchdog = (): void => {
    clearStallWatchdog()
    positionStallTimer = setTimeout(() => {
      positionStallTimer = null
      const { state: liveState, currentTrack: liveTrack } = useAudioStore.getState()
      if (liveState !== 'playing') return
      console.warn(
        `[audio-diag] sink_stage_stall track_id=${shortId(
          liveTrack?.id ?? ''
        )} state=playing no_position_events_ms=10000`
      )
    }, 10000)
  }

  // Listen for audio events from the Rust engine.
  useEffect(() => {
    let unsubscribe: (() => void) | null = null

    async function init() {
      // Hydrate the UI from the real engine state instead of assuming stopped.
      try {
        const initial = await invoke<PlaybackState>('audio_get_state')
        setState(initial)
      } catch {
        // Engine may not be started yet; stay at store default.
      }

      unsubscribe = await listen<AudioEvent>('audio_event', (event) => {
        const { type, data } = event.payload
        // Read live store values so diagnostics are not stale from effect closure.
        const { state: liveState, currentTrack: liveTrack } = useAudioStore.getState()
        switch (type) {
          case 'state_changed':
            diagCount('stateChangedEvents')
            if (data === 'playing') {
              diagEnd('frontend_state_playing', true)
              // Count a play as successful only when the engine confirms
              // Playing; a silent sink failure emits an error event instead,
              // so playSuccess never fires on a failed sink.
              diagCount('playSuccess')
              startStallWatchdog()
            } else {
              clearStallWatchdog()
            }
            setState(data as PlaybackState)
            break
          case 'track_changed':
            setCurrentTrack(data as TrackHandle | null)
            break
          case 'position_changed': {
            const payload = data as { position: number; duration: number }
            setPosition(payload.position)
            setDuration(payload.duration)
            diagCount('positionEvents')
            // Position advancement confirms the sink is consuming audio.
            clearStallWatchdog()
            if (liveState === 'playing') startStallWatchdog()
            break
          }
          case 'volume_changed':
            setVolume(data as number)
            break
          case 'error': {
            diagCount('errorEvents')
            const payload = data as AudioErrorData | undefined
            const message = payload?.message ?? 'Audio error'
            const errorCategory =
              message.startsWith('Decoder error') ? 'decode'
              : message.startsWith('Output device error') ? 'sink'
              : 'unknown'
            if (errorCategory === 'decode') diagCount('decodeFailures')
            else if (errorCategory === 'sink') diagCount('sinkFailures')
            // Flag a potential false-success: engine reports error while UI
            // still believes it is playing (sink-stage failure after decode).
            if (liveState === 'playing') {
              console.warn(
                `[audio-diag] error_received_while_playing track_id=${shortId(
                  liveTrack?.id ?? ''
                )} error_category=${errorCategory}`
              )
              diagEnd('frontend_state_playing', false, errorCategory)
            }
            clearStallWatchdog()
            setState('error')
            setError(message)
            break
          }
          case 'track_ended':
            setState('ended')
            break
        }
      })
    }

    init()

    return () => {
      clearStallWatchdog()
      unsubscribe?.()
    }
  }, [
    setState,
    setCurrentTrack,
    setPosition,
    setDuration,
    setVolume,
    setError,
  ])

  const getState = useCallback(async (): Promise<PlaybackState> => {
    const playbackState = await invoke<PlaybackState>('audio_get_state')
    setState(playbackState)
    return playbackState
  }, [setState])

  const play = useCallback(async (track: TrackHandle) => {
    setError(null)
    diagCount('playCalls')
    diagStart('audio_play_invoke')
    try {
      await invoke('audio_play', { trackHandle: track })
      diagEnd('audio_play_invoke', true)
      diagStart('frontend_state_playing')
    } catch (error) {
      diagEnd('audio_play_invoke', false, 'invoke_error')
      diagCount('playFailure')
      console.error('Play failed:', error)
      setError(String(error))
      throw error
    }
  }, [setError])

  const pause = useCallback(async () => {
    setError(null)
    try {
      await invoke('audio_pause')
    } catch (error) {
      console.error('Pause failed:', error)
      setError(String(error))
    }
  }, [setError])

  const resume = useCallback(async () => {
    setError(null)
    try {
      await invoke('audio_resume')
    } catch (error) {
      console.error('Resume failed:', error)
      setError(String(error))
    }
  }, [setError])

  const stop = useCallback(async () => {
    setError(null)
    try {
      await invoke('audio_stop')
    } catch (error) {
      console.error('Stop failed:', error)
      setError(String(error))
    }
  }, [setError])

  const seek = useCallback(async (positionMs: number) => {
    setError(null)
    try {
      await invoke('audio_seek', { positionMs })
    } catch (error) {
      console.error('Seek failed:', error)
      setError(String(error))
    }
  }, [setError])

  const setVolumeLevel = useCallback(async (vol: number) => {
    setError(null)
    try {
      await invoke('audio_set_volume', { volume: vol })
    } catch (error) {
      console.error('Set volume failed:', error)
      setError(String(error))
    }
  }, [setError])

  return {
    state,
    currentTrack,
    position,
    duration,
    volume,
    error,
    isPlaying: state === 'playing',
    isPaused: state === 'paused',
    isBuffering: state === 'buffering',
    getState,
    play,
    pause,
    resume,
    stop,
    seek,
    setVolume: setVolumeLevel,
  }
}
