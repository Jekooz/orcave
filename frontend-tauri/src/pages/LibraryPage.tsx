import { useState } from 'react'
import { clsx } from 'clsx'
import { Folder, Music, Users, Album, RefreshCw, Play, AlertCircle } from 'lucide-react'
import { SectionTitle } from '@/components/SectionTitle'
import { useLibrary } from '@/hooks/useLibrary'

export function LibraryPage() {
  const {
    tracks,
    artists,
    albums,
    loading,
    error,
    refresh,
    addFolder,
    rescan,
    playTrack,
  } = useLibrary()
  const [folderPath, setFolderPath] = useState('')
  const [activeTab, setActiveTab] = useState<'tracks' | 'artists' | 'albums'>('tracks')

  const formatDuration = (ms: number | null) => {
    if (!ms) return '--:--'
    const seconds = Math.floor(ms / 1000)
    const mins = Math.floor(seconds / 60)
    const secs = seconds % 60
    return `${mins}:${secs.toString().padStart(2, '0')}`
  }

  const handleAddFolder = async (e: React.FormEvent) => {
    e.preventDefault()
    const path = folderPath.trim()
    if (!path) return
    try {
      await addFolder(path)
      setFolderPath('')
    } catch {
      // error surfaced by hook
    }
  }

  const tabs = [
    { id: 'tracks', label: 'Tracks', icon: Music },
    { id: 'artists', label: 'Artists', icon: Users },
    { id: 'albums', label: 'Albums', icon: Album },
  ] as const

  return (
    <div className="space-y-6">
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
        <div>
          <SectionTitle>Local Library</SectionTitle>
          <p className="text-orcave-muted">
            {tracks.length} track{tracks.length !== 1 ? 's' : ''} in library
          </p>
        </div>

        <div className="flex items-center gap-2 w-full sm:w-auto">
          <form onSubmit={handleAddFolder} className="flex gap-2 w-full sm:w-80" aria-label="Add library folder">
            <input
              type="text"
              aria-label="Folder path"
              name="folderPath"
              value={folderPath}
              onChange={(e) => setFolderPath(e.target.value)}
              placeholder="C:\Music or \\server\share"
              className="input flex-1"
              disabled={loading}
            />
            <button
              type="submit"
              disabled={loading || !folderPath.trim()}
              className="btn-primary"
              aria-label="Add library folder"
            >
              <Folder className="w-4 h-4 mr-1" />
              Add folder
            </button>
          </form>
          <button
            onClick={() => void refresh()}
            disabled={loading}
            className="btn-secondary"
            aria-label="Refresh library"
          >
            <RefreshCw className="w-4 h-4 mr-1" />
            Refresh
          </button>
          <button
            onClick={() => void rescan()}
            disabled={loading}
            className="btn-secondary"
            aria-label="Rescan library folders"
          >
            Rescan
          </button>
        </div>
      </div>

      {error && (
        <p role="alert" className="p-3 rounded-lg bg-orcave-error/10 border border-orcave-error/30 text-orcave-error text-sm flex items-center gap-2">
          <AlertCircle className="w-4 h-4 shrink-0" aria-hidden="true" />
          Library error: {error}
        </p>
      )}

      {/* Tab Navigation */}
      <div className="flex gap-1 overflow-x-auto pb-2">
        {tabs.map((tab) => {
          const Icon = tab.icon
          const isActive = activeTab === tab.id
          return (
            <button
              key={tab.id}
              onClick={() => setActiveTab(tab.id)}
              aria-pressed={isActive}
              className={clsx(
                'flex items-center gap-2 px-4 py-2 rounded-lg text-sm font-medium whitespace-nowrap transition-colors',
                isActive
                  ? 'bg-orcave-primary text-orcave-bg'
                  : 'text-orcave-muted hover:text-orcave-text hover:bg-orcave-surfaceElevated'
              )}
            >
              <Icon className="w-4 h-4 flex-shrink-0" />
              {tab.label}
            </button>
          )
        })}
      </div>

      {loading && (
        <div className="flex items-center justify-center h-64">
          <span className="sr-only">Loading library</span>
        </div>
      )}

      {!loading && activeTab === 'tracks' && (
        <section aria-label="Library tracks">
          {tracks.length === 0 && !error && (
            <div className="card p-12 text-center text-orcave-muted">
              <Music className="w-16 h-16 mx-auto mb-4 opacity-50" aria-hidden="true" />
              <h3 className="font-medium mb-2">No tracks in library</h3>
              <p>Add a music folder above to scan for local tracks. Played tracks appear in the Queue and Now Playing.</p>
            </div>
          )}
          {tracks.length > 0 && (
            <div className="divide-y divide-orcave-border/50">
              {tracks.map((track) => (
                <div
                  key={track.id}
                  className="flex items-center gap-3 p-3 hover:bg-orcave-surfaceElevated transition-colors"
                >
                  <div className="flex items-center gap-3 flex-1 min-w-0">
                    {track.artwork_path ? (
                      <img
                        src={track.artwork_path}
                        alt=""
                        className="w-10 h-10 rounded object-cover"
                        loading="lazy"
                      />
                    ) : (
                      <div className="w-10 h-10 rounded bg-orcave-surfaceElevated flex items-center justify-center text-orcave-muted">
                        <Music className="w-5 h-5" aria-hidden="true" />
                      </div>
                    )}
                    <div className="flex-1 min-w-0">
                      <p className="font-medium truncate" title={track.title}>
                        {track.title}
                      </p>
                      <p className="text-sm text-orcave-muted truncate" title={track.artist}>
                        {track.artist}
                      </p>
                      {track.album && (
                        <p className="text-xs text-orcave-muted/70 truncate" title={track.album}>
                          {track.album}
                        </p>
                      )}
                    </div>
                    <span className="text-orcave-muted text-sm w-16 shrink-0 text-right">
                      {formatDuration(track.duration)}
                    </span>
                    <button
                      onClick={() => void playTrack(track).catch(console.error)}
                      className="btn-ghost p-1.5"
                      aria-label={`Play ${track.title}`}
                    >
                      <Play className="w-4 h-4" aria-hidden="true" />
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </section>
      )}

      {!loading && activeTab === 'artists' && (
        <section aria-label="Library artists">
          {artists.length === 0 && !error && (
            <div className="card p-12 text-center text-orcave-muted">
              <p>No artists yet. Add a folder and scan the library.</p>
            </div>
          )}
          {artists.length > 0 && (
            <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-4">
              {artists.map((artist) => (
                <div key={artist.id} className="card p-4 text-center">
                  <Users className="w-12 h-12 mx-auto mb-3 text-orcave-muted" aria-hidden="true" />
                  <p className="font-medium truncate">{artist.name}</p>
                  <p className="text-xs text-orcave-muted">{artist.track_ids.length} track{artist.track_ids.length !== 1 ? 's' : ''}</p>
                </div>
              ))}
            </div>
          )}
        </section>
      )}

      {!loading && activeTab === 'albums' && (
        <section aria-label="Library albums">
          {albums.length === 0 && !error && (
            <div className="card p-12 text-center text-orcave-muted">
              <p>No albums yet. Add a folder and scan the library.</p>
            </div>
          )}
          {albums.length > 0 && (
            <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-3 xl:grid-cols-4 gap-4">
              {albums.map((album) => (
                <div key={album.id} className="card p-3">
                  <div className="aspect-square bg-orcave-surfaceElevated rounded-lg flex items-center justify-center mb-3">
                    <Album className="w-8 h-8 text-orcave-muted" aria-hidden="true" />
                  </div>
                  <p className="font-medium text-sm truncate">{album.title}</p>
                  <p className="text-xs text-orcave-muted">{album.artist}</p>
                </div>
              ))}
            </div>
          )}
        </section>
      )}
    </div>
  )
}
