import { useQueue } from '@/hooks/useQueue'
import { useAudio } from '@/hooks/useAudio'
import { Trash2, Pause, Shuffle, Repeat } from 'lucide-react'
import { useState } from 'react'
import { clsx } from 'clsx'

export function QueuePage() {
  const { queue, history, currentIndex, remove, move, clear, playIndex, setShuffle, shuffle, setRepeat, repeatMode } = useQueue()
  const { state, play } = useAudio()
  const [draggedIndex, setDraggedIndex] = useState<number | null>(null)

  const handleDragStart = (e: React.DragEvent, index: number) => {
    setDraggedIndex(index)
    e.dataTransfer.effectAllowed = 'move'
  }

  const handleDragOver = (e: React.DragEvent) => {
    e.preventDefault()
    e.dataTransfer.dropEffect = 'move'
  }

  const handleDrop = (e: React.DragEvent, index: number) => {
    e.preventDefault()
    if (draggedIndex !== null && draggedIndex !== index) {
      move(draggedIndex, index).catch(console.error)
    }
    setDraggedIndex(null)
  }

  const handleDragEnd = () => {
    setDraggedIndex(null)
  }

  return (
    <div className="space-y-6">
      {/* Queue Header */}
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
        <div>
          <h1 className="text-2xl font-bold">Play Queue</h1>
          <p className="text-orcave-muted">{queue.length} tracks • {history.length} in history</p>
        </div>

        <div className="flex items-center gap-2">
          <button
            onClick={() => setShuffle(!shuffle)}
            aria-pressed={shuffle}
            className={clsx('btn-secondary', shuffle && 'bg-orcave-primary/20 text-orcave-primary')}
          >
            <Shuffle className="w-4 h-4 mr-1" /> Shuffle
          </button>
          <button
            onClick={() => setRepeat(repeatMode === 'off' ? 'all' : repeatMode === 'all' ? 'one' : 'off')}
            aria-label={`Repeat: ${repeatMode}`}
            className={clsx('btn-secondary', repeatMode !== 'off' && 'bg-orcave-primary/20 text-orcave-primary')}
          >
            <Repeat className="w-4 h-4 mr-1" />
            {repeatMode === 'off' ? 'Off' : repeatMode === 'all' ? 'All' : 'One'}
          </button>
          {queue.length > 0 && (
            <button onClick={clear} className="btn-secondary text-orcave-error hover:bg-orcave-error/10">
              Clear
            </button>
          )}
        </div>
      </div>

      {/* Current Track */}
      {queue[currentIndex] && (
        <div className="card p-4 bg-orcave-primary/10 border-orcave-primary/30">
          <div className="flex items-center gap-4">
            <div className="flex items-center justify-center w-10 h-10 rounded bg-orcave-primary text-orcave-bg">
              <Pause className="w-5 h-5" />
            </div>
            <div className="flex-1 min-w-0">
              <p className="font-medium truncate">{queue[currentIndex].media_item.title}</p>
              <p className="text-sm text-orcave-muted truncate">{queue[currentIndex].media_item.artists[0]?.name}</p>
            </div>
            <span className="badge badge-primary">{state === 'playing' ? 'Playing' : 'Selected'}</span>
          </div>
        </div>
      )}

      {/* Up Next */}
      <div className="card">
        <div className="p-4 border-b border-orcave-border flex items-center justify-between">
          <h2 className="text-lg font-semibold">Up Next ({queue.length - currentIndex - 1})</h2>
        </div>

        {queue.length > currentIndex + 1 ? (
          <div className="divide-y divide-orcave-border/50">
            {queue.slice(currentIndex + 1).map((item, index) => {
              const actualIndex = currentIndex + 1 + index
              return (
                <div
                  key={item.queue_id}
                  draggable
                  onDragStart={(e) => handleDragStart(e, actualIndex)}
                  onDragOver={handleDragOver}
                  onDrop={(e) => handleDrop(e, actualIndex)}
                  onDragEnd={handleDragEnd}
                  className={clsx(
                    'flex items-center gap-3 p-3 hover:bg-orcave-surfaceElevated transition-colors',
                    draggedIndex === actualIndex && 'opacity-50 bg-orcave-primary/10'
                  )}
                >
                  <div className="flex items-center justify-center w-8 text-orcave-muted text-sm">
                    <span>{actualIndex + 1}</span>
                  </div>

                  {item.media_item.thumbnails[0] && (
                    <img
                      src={item.media_item.thumbnails[0].url}
                      alt=""
                      className="w-12 h-12 rounded object-cover"
                    />
                  )}

                  <div className="flex-1 min-w-0">
                    <p className="font-medium truncate">{item.media_item.title}</p>
                    <p className="text-sm text-orcave-muted truncate">{item.media_item.artists[0]?.name}</p>
                  </div>

                  <button
                    onClick={() => playIndex(actualIndex)}
                    className="btn-ghost p-1.5"
                  >Play</button>
                  <button
                    onClick={() => remove(actualIndex)}
                    className="btn-ghost p-1.5 opacity-0 group-hover:opacity-100 text-orcave-error hover:bg-orcave-error/10"
                    aria-label="Remove from queue"
                  >
                    <Trash2 className="w-4 h-4" />
                  </button>
                </div>
              )
            })}
          </div>
        ) : (
          <div className="p-8 text-center text-orcave-muted">
            <p>No tracks in queue</p>
            <p className="text-sm mt-1">Add music from the Catalogue tab</p>
          </div>
        )}
      </div>

      {/* History */}
      {history.length > 0 && (
        <div className="card">
          <div className="p-4 border-b border-orcave-border">
            <h2 className="text-lg font-semibold">History ({history.length})</h2>
          </div>
          <div className="max-h-64 overflow-y-auto divide-y divide-orcave-border/50">
            {history.slice(0, 20).map((item) => (
              <button
                key={item.queue_id}
                onClick={() => play(item.track_handle)}
                className="w-full flex items-center gap-3 p-3 hover:bg-orcave-surfaceElevated text-left transition-colors"
              >
                {item.media_item.thumbnails[0] && (
                  <img src={item.media_item.thumbnails[0].url} alt="" className="w-10 h-10 rounded object-cover" />
                )}
                <div className="flex-1 min-w-0">
                  <p className="font-medium truncate">{item.media_item.title}</p>
                  <p className="text-sm text-orcave-muted truncate">{item.media_item.artists[0]?.name}</p>
                </div>
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  )
}
