import { useAudio } from '@/hooks/useAudio'
import { useQueue } from '@/hooks/useQueue'
import { Play, Pause, SkipBack, SkipForward, Volume2, VolumeX, Shuffle, Repeat, Music, AlertCircle, Loader2 } from 'lucide-react'
import { clsx } from 'clsx'

export function PlaybackPage() {
  const {
    currentTrack,
    position,
    duration,
    volume,
    error,
    isPlaying,
    isBuffering,
    resume,
    pause,
    seek,
    setVolume,
    stop,
  } = useAudio()
  const { queue, currentIndex, next, previous, setRepeat, setShuffle, repeatMode, shuffle } = useQueue()

  const formatTime = (ms: number) => {
    const seconds = Math.floor(ms / 1000)
    const mins = Math.floor(seconds / 60)
    const secs = seconds % 60
    return `${mins}:${secs.toString().padStart(2, '0')}`
  }

  const handleSeek = (e: React.ChangeEvent<HTMLInputElement>) => {
    const value = parseFloat(e.target.value)
    seek(value)
  }

  const handleVolumeChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const value = parseFloat(e.target.value)
    setVolume(value)
  }

  const queueItem = queue[currentIndex] || null
  const artwork = queueItem?.media_item.thumbnails[0]?.url
  const trackTitle = currentTrack?.title || queueItem?.media_item.title || 'Nothing playing'
  const trackArtist = currentTrack?.artist || queueItem?.media_item.artists[0]?.name || 'Unknown artist'
  const trackAlbum = currentTrack ? (queueItem?.media_item.album?.name || 'Unknown album') : null

  return (
    <div className="space-y-6 max-w-4xl mx-auto">
      {/* Playback error state */}
      {error && (
        <div role="alert" className="card p-4 border-orcave-error/30 bg-orcave-error/10 flex items-center gap-3">
          <AlertCircle className="w-5 h-5 text-orcave-error shrink-0" aria-hidden="true" />
          <div className="flex-1 min-w-0">
            <p className="font-medium text-orcave-text">Playback error</p>
            <p className="text-sm text-orcave-error">{error}</p>
          </div>
          <button
            onClick={() => void stop()}
            className="btn-secondary text-sm"
          >
            Stop
          </button>
        </div>
      )}

      {/* Now Playing Card */}
      <div className="card p-6">
        <div className="flex flex-col md:flex-row gap-6 items-center md:items-start">
          <div className="relative w-48 h-48 md:w-64 md:h-64 flex-shrink-0 rounded-lg overflow-hidden bg-orcave-surfaceElevated">
            {artwork ? (
              <img src={artwork} alt="" className="w-full h-full object-cover" />
            ) : (
              <div className="w-full h-full flex items-center justify-center text-orcave-muted">
                <Music className="w-16 h-16" aria-hidden="true" />
              </div>
            )}
            {isBuffering && (
              <div className="absolute inset-0 flex items-center justify-center bg-black/40">
                <Loader2 className="w-8 h-8 text-white animate-spin" aria-hidden="true" />
                <span className="sr-only">Buffering</span>
              </div>
            )}
          </div>

          <div className="flex-1 min-w-0 text-center md:text-left">
            <p className="text-xs text-orcave-muted uppercase tracking-wider mb-1">Now Playing</p>
            <h1 className="text-2xl font-bold truncate mb-1">{trackTitle}</h1>
            <p className="text-orcave-muted mb-1">{trackArtist}</p>
            {trackAlbum && <p className="text-sm text-orcave-muted/70 truncate">{trackAlbum}</p>}
            {isBuffering && (
              <p className="text-xs text-orcave-primary mt-1 flex items-center gap-1">
                <span className="relative inline-block h-[8px] w-[8px] rounded-full bg-orcave-primary animate-pulse" aria-hidden="true" />
                Buffering
              </p>
            )}
          </div>

          <div className="ml-auto flex items-center gap-2">
            {currentTrack ? (
              <button
                onClick={() => void stop()}
                className="btn-secondary text-sm"
                aria-label="Stop playback"
              >
                Stop
              </button>
            ) : null}
          </div>
        </div>

        {/* Progress Bar */}
        <div className="mt-6">
          <div className="flex items-center gap-3">
            <span className="text-xs text-orcave-muted w-12 text-right">{duration ? formatTime(position) : '--:--'}</span>
            <input
              type="range"
              aria-label="Playback position"
              className="flex-1 accent-orcave-primary"
              min={0}
              max={Math.max(duration, 1)}
              value={Math.min(position, Math.max(duration, 1))}
              onChange={handleSeek}
              disabled={!duration}
            />
            <span className="text-xs text-orcave-muted w-12">{duration ? formatTime(duration) : '--:--'}</span>
          </div>
        </div>

        {/* Controls */}
        <div className="flex items-center justify-center gap-6 mt-6">
          <button
            onClick={() => setShuffle(!shuffle)}
            className={clsx('btn-ghost p-2', shuffle && 'text-orcave-primary')}
            aria-label="Shuffle"
            aria-pressed={shuffle}
            disabled={!currentTrack}
          >
            <Shuffle className="w-5 h-5" />
          </button>

          <button onClick={previous} className="btn-ghost p-3" aria-label="Previous" disabled={!currentTrack}>
            <SkipBack className="w-6 h-6" />
          </button>

          <button
            onClick={isPlaying ? pause : resume}
            disabled={!currentTrack}
            className="btn-primary p-4 rounded-full shadow-lg"
            aria-label={isPlaying ? 'Pause' : 'Play'}
          >
            {isPlaying ? <Pause className="w-6 h-6" /> : <Play className="w-6 h-6 ml-1" />}
          </button>

          <button onClick={next} className="btn-ghost p-3" aria-label="Next" disabled={!currentTrack}>
            <SkipForward className="w-6 h-6" />
          </button>

          <button
            onClick={() => setRepeat(repeatMode === 'off' ? 'all' : repeatMode === 'all' ? 'one' : 'off')}
            className={clsx('btn-ghost p-2', repeatMode !== 'off' && 'text-orcave-primary')}
            aria-label={`Repeat: ${repeatMode}`}
            aria-pressed={repeatMode !== 'off'}
            disabled={!currentTrack}
          >
            <Repeat className={clsx('w-5 h-5', repeatMode === 'one' && 'text-orcave-primary')} />
          </button>
        </div>

        {/* Volume */}
        <div className="flex items-center justify-center gap-3 mt-4">
          {volume === 0 ? <VolumeX aria-hidden="true" /> : <Volume2 aria-hidden="true" />}
          <div className="flex items-center gap-2">
            <input
              type="range"
              aria-label="Volume"
              min="0"
              max="1"
              step="0.01"
              value={volume}
              onChange={handleVolumeChange}
              className="w-32 h-2 appearance-none bg-orcave-border rounded-full accent-orcave-primary"
              disabled={!currentTrack}
            />
            <span className="text-xs text-orcave-muted w-8">{Math.round(volume * 100)}%</span>
          </div>
        </div>
      </div>

      {/* Track changed announcement for screen readers */}
      {currentTrack && (
        <p className="sr-only" aria-live="polite">
          Now playing {currentTrack.title} by {currentTrack.artist}
        </p>
      )}

      {/* Queue Preview */}
      <div className="card">
        <div className="p-4 border-b border-orcave-border flex items-center justify-between">
          <h2 className="text-lg font-semibold">Up Next</h2>
          <span className="text-sm text-orcave-muted">{queue.length} tracks</span>
        </div>
        <div className="max-h-64 overflow-y-auto">
          {queue.slice(currentIndex + 1, currentIndex + 6).map((item, index) => (
            <div key={item.queue_id} className="flex items-center gap-3 px-4 py-2 hover:bg-orcave-surfaceElevated">
              <span className="text-orcave-muted text-sm w-6">{currentIndex + index + 2}</span>
              {item.media_item.thumbnails[0] && (
                <img src={item.media_item.thumbnails[0].url} alt="" className="w-10 h-10 rounded object-cover" />
              )}
              <div className="flex-1 min-w-0">
                <p className="text-sm font-medium truncate">{item.media_item.title}</p>
                <p className="text-xs text-orcave-muted truncate">{item.media_item.artists[0]?.name}</p>
              </div>
            </div>
          ))}
          {queue.length <= currentIndex + 1 && (
            <div className="p-4 text-center text-orcave-muted">Queue is empty</div>
          )}
        </div>
      </div>
    </div>
  )
}
