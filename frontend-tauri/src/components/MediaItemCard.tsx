import { Play, Music, Users, Album } from 'lucide-react'
import { useState } from 'react'
import { invoke } from '@/lib/tauri'
import type { MediaItem } from '@/lib/tauri'

interface MediaItemCardProps {
  item: MediaItem
}

export function MediaItemCard({ item }: MediaItemCardProps) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')

  const getIcon = () => {
    switch (item.media_type) {
      case 'track': return <Music className="w-5 h-5" />
      case 'album': return <Album className="w-5 h-5" />
      case 'artist': return <Users className="w-5 h-5" />
      case 'playlist': return <Music className="w-5 h-5" />
      default: return <Music className="w-5 h-5" />
    }
  }

  const thumbnail = item.thumbnails[0]?.url

  const handlePlay = async () => {
    if (item.media_type !== 'track') return
    setBusy(true)
    setError('')
    try {
      // Register the local file:// stream URL on the Rust AudioEngine before
      // playback. Without this, stream_urls[id] is empty and play() returns
      // TrackNotFound before any decode happens (silent playback).
      await invoke('library_register_track', { trackId: item.id })
      await invoke('audio_play', {
        trackHandle: {
          id: item.id,
          title: item.title,
          artist: item.artists.map((artist) => artist.name).join(', '),
        },
      })
    } catch (error) {
      console.error('Play failed:', error)
      setError(String(error))
    } finally {
      setBusy(false)
    }
  }

  const formatDuration = (ms?: number) => {
    if (!ms) return ''
    const seconds = Math.floor(ms / 1000)
    const mins = Math.floor(seconds / 60)
    const secs = seconds % 60
    return `${mins}:${secs.toString().padStart(2, '0')}`
  }

  return (
    <div className="card group">
      <div className="relative aspect-square overflow-hidden">
        {thumbnail ? (
          <img
            src={thumbnail}
            alt={item.title}
            className="w-full h-full object-cover transition-transform duration-300 group-hover:scale-105"
            loading="lazy"
          />
        ) : (
          <div className="w-full h-full bg-orcave-surfaceElevated flex items-center justify-center">
            {getIcon()}
          </div>
        )}

        {item.media_type === 'track' && <div className="absolute inset-0 bg-black/50 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 focus-within:opacity-100 transition-opacity duration-200 flex items-center justify-center">
          <button
            onClick={handlePlay}
            disabled={busy}
            className="btn-primary p-3 rounded-full shadow-lg transform group-hover:scale-105 transition-transform"
            aria-label={`Play ${item.title}`}
          >
            <Play className="w-5 h-5 ml-1" />
          </button>
        </div>}

        {item.duration && (
          <div className="absolute bottom-2 right-2 bg-black/80 text-white text-xs px-1.5 py-0.5 rounded">
            {formatDuration(item.duration)}
          </div>
        )}

        {item.is_explicit && (
          <div className="absolute top-2 left-2 bg-orcave-bg/80 text-orcave-text text-xs px-1 py-0.5 rounded font-medium">
            E
          </div>
        )}
      </div>

      <div className="p-3 space-y-1">
        <h3 className="font-medium text-sm truncate group-hover:text-orcave-primary transition-colors">
          {item.title}
        </h3>
        {item.subtitle && (
          <p className="text-xs text-orcave-muted truncate">{item.subtitle}</p>
        )}
        {item.artists.length > 0 && (
          <p className="text-xs text-orcave-muted/70 truncate">
            {item.artists.map((a) => a.name).join(', ')}
          </p>
        )}
        {error && <p role="status" className="text-xs text-orcave-error">{error}</p>}
      </div>
    </div>
  )
}
