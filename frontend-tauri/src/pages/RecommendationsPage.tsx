import { useRecommendations } from '@/hooks/useRecommendations'
import { useCatalogue, BROWSE_IDS } from '@/hooks/useCatalogue'
import { MediaItemCard } from '@/components/MediaItemCard'
import { SectionTitle } from '@/components/SectionTitle'
import { Zap, Sparkles, Music, Radio, Loader2, RefreshCw } from 'lucide-react'
import { useState, useEffect } from 'react'
import { clsx } from 'clsx'
import type { MixType, StationType, SeedType, MediaItem } from '@/lib/tauri'

export function RecommendationsPage() {
  const { currentSet, mixes, stations, loading, getForSeed, getMix, getStation } = useRecommendations()
  const { browse } = useCatalogue()
  const [activeTab, setActiveTab] = useState<'mixes' | 'stations' | 'seeds'>('mixes')
  const [selectedSeed, setSelectedSeed] = useState<{ type: SeedType; id: string; item: MediaItem } | null>(null)
  const [seedCandidates, setSeedCandidates] = useState<MediaItem[]>([])
  const [seedType, setSeedType] = useState<SeedType>('track')
  const [error, setError] = useState('')

  const loadSeeds = async (type: SeedType) => {
    setSeedType(type)
    setError('')
    try {
      const result = await browse(BROWSE_IDS[type === 'track' ? 'history' : 'explore'])
      setSeedCandidates(result.sections.flatMap((section) => section.items).filter((item) => item.media_type === type))
    } catch (reason) {
      setError(String(reason))
    }
  }

  const selectSeed = async (item: MediaItem) => {
    setSelectedSeed({ type: seedType, id: item.id, item })
    try {
      await getForSeed(seedType, item.id)
      setError('')
    } catch (reason) {
      setError(String(reason))
    }
  }

  const mixTypes: { id: MixType; label: string; icon: React.ComponentType<{ className?: string }> }[] = [
    { id: 'your_mix', label: 'Your Mix', icon: Music },
    { id: 'discover_mix', label: 'Discover Mix', icon: Sparkles },
    { id: 'new_release_mix', label: 'New Releases', icon: Radio },
    { id: 'liked_songs_mix', label: 'Liked Songs Mix', icon: Music },
    { id: 'supermix', label: 'Supermix', icon: Zap },
  ]

  const stationTypes: { id: StationType; label: string; icon: React.ComponentType<{ className?: string }> }[] = [
    { id: 'focus', label: 'Focus', icon: Radio },
    { id: 'workout', label: 'Workout', icon: Zap },
    { id: 'relax', label: 'Relax', icon: Music },
    { id: 'party', label: 'Party', icon: Sparkles },
    { id: 'commute', label: 'Commute', icon: Radio },
    { id: 'sleep', label: 'Sleep', icon: Music },
  ]

  const loadMix = async (mixType: MixType) => {
    try {
      await getMix(mixType)
    } catch (error) {
      console.error('Failed to load mix:', error)
    }
  }

  const loadStation = async (stationType: StationType) => {
    try {
      await getStation(stationType)
    } catch (error) {
      console.error('Failed to load station:', error)
    }
  }

  // Load default mix on mount
  useEffect(() => {
    loadMix('your_mix')
  }, [])

  return (
    <div className="space-y-6">
      {error && <p role="status">{error}</p>}
      {/* Tab Navigation */}
      <div className="flex gap-1 overflow-x-auto pb-2">
        {['mixes', 'stations', 'seeds'].map((tab) => (
          <button
            key={tab}
            onClick={() => setActiveTab(tab as typeof activeTab)}
            className={clsx(
              'px-4 py-2 rounded-lg text-sm font-medium whitespace-nowrap transition-colors',
              activeTab === tab
                ? 'bg-orcave-primary text-orcave-bg'
                : 'text-orcave-muted hover:text-orcave-text hover:bg-orcave-surfaceElevated'
            )}
          >
            {tab.charAt(0).toUpperCase() + tab.slice(1)}
          </button>
        ))}
      </div>

      {/* Mixes Tab */}
      {activeTab === 'mixes' && (
        <div className="space-y-6">
          {mixTypes.map((mix) => {
            const Icon = mix.icon
            const mixData = mixes[mix.id]
            const isLoading = loading && currentSet?.algorithm === 'personalized_mix'

            return (
              <section key={mix.id} className="animate-in">
                <div className="flex items-center justify-between mb-4">
                  <SectionTitle>
                    <Icon className="w-5 h-5 inline-block mr-2" />
                    {mix.label}
                  </SectionTitle>
                  <button
                    onClick={() => loadMix(mix.id)}
                    disabled={isLoading}
                    className="btn-secondary text-sm"
                  >
                    {isLoading ? (
                      <>
                        <Loader2 className="w-4 h-4 animate-spin" />
                        Loading...
                      </>
                    ) : (
                      <>
                        <RefreshCw className="w-4 h-4" />
                        Refresh
                      </>
                    )}
                  </button>
                </div>

                {mixData && mixData.items.length > 0 ? (
                  <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-4">
                    {mixData.items.map((item) => (
                      <MediaItemCard key={item.id} item={item} />
                    ))}
                  </div>
                ) : (
                  <div className="card p-8 text-center text-orcave-muted">
                    {isLoading ? (
                      <Loader2 className="w-8 h-8 text-orcave-primary animate-spin mx-auto mb-2" />
                    ) : (
                      <button onClick={() => loadMix(mix.id)} className="btn-primary">
                        Load {mix.label}
                      </button>
                    )}
                  </div>
                )}
              </section>
            )
          })}
        </div>
      )}

      {/* Stations Tab */}
      {activeTab === 'stations' && (
        <div className="space-y-6">
          {stationTypes.map((station) => {
            const Icon = station.icon
            const stationKey = typeof station.id === 'string' ? station.id : `genre:${station.id.genre}`
            const stationData = stations[stationKey]
            const isLoading = loading && currentSet?.algorithm === 'mood_station'

            return (
              <section key={stationKey} className="animate-in">
                <div className="flex items-center justify-between mb-4">
                  <SectionTitle>
                    <Icon className="w-5 h-5 inline-block mr-2" />
                    {station.label} Station
                  </SectionTitle>
                  <button
                    onClick={() => loadStation(station.id as StationType)}
                    disabled={isLoading}
                    className="btn-secondary text-sm"
                  >
                    {isLoading ? 'Loading...' : 'Refresh'}
                  </button>
                </div>

                {stationData && stationData.items.length > 0 ? (
                  <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-4">
                    {stationData.items.map((item) => (
                      <MediaItemCard key={item.id} item={item} />
                    ))}
                  </div>
                ) : (
                  <div className="card p-8 text-center text-orcave-muted">
                    {isLoading ? (
                      <Loader2 className="w-8 h-8 text-orcave-primary animate-spin mx-auto mb-2" />
                    ) : (
                      <button onClick={() => loadStation(station.id as StationType)} className="btn-primary">
                        Load {station.label} Station
                      </button>
                    )}
                  </div>
                )}
              </section>
            )
          })}
        </div>
      )}

      {/* Seed-based Recommendations Tab */}
      {activeTab === 'seeds' && (
        <div className="space-y-6">
          <section className="animate-in">
            <SectionTitle>Seed-based Recommendations</SectionTitle>
            <p className="text-orcave-muted mb-4">
              Select a track, album, artist, or playlist from the Catalogue to see personalized recommendations.
            </p>

            {selectedSeed && (
              <div className="card p-4 mb-4 bg-orcave-primary/10 border-orcave-primary/30">
                <div className="flex items-center gap-3">
                  {selectedSeed.item.thumbnails[0] && (
                    <img src={selectedSeed.item.thumbnails[0].url} alt="" className="w-12 h-12 rounded object-cover" />
                  )}
                  <div>
                    <p className="font-medium">{selectedSeed.item.title}</p>
                    <p className="text-sm text-orcave-muted capitalize">{selectedSeed.type} • {selectedSeed.item.artists[0]?.name}</p>
                  </div>
                   <button aria-label="Clear selected seed" onClick={() => setSelectedSeed(null)} className="ml-auto btn-ghost p-1">
                    ✕
                  </button>
                </div>

                {currentSet && currentSet.seed_id === selectedSeed.id && currentSet.items.length > 0 && (
                  <div className="mt-4">
                    <h4 className="font-medium mb-2">Recommendations ({currentSet.items.length})</h4>
                    <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-3">
                      {currentSet.items.slice(0, 8).map((item) => (
                        <MediaItemCard key={item.id} item={item} />
                      ))}
                    </div>
                  </div>
                )}

                {currentSet && currentSet.seed_id === selectedSeed.id && loading && (
                  <div className="mt-4 text-center">
                    <Loader2 className="w-8 h-8 text-orcave-primary animate-spin mx-auto" />
                  </div>
                )}
              </div>
            )}

            {!selectedSeed && (
              <div className="grid grid-cols-2 sm:grid-cols-4 gap-4">
                {[
                  { type: 'track', label: 'From Track', icon: Music },
                  { type: 'album', label: 'From Album', icon: Radio },
                  { type: 'artist', label: 'From Artist', icon: Sparkles },
                  { type: 'playlist', label: 'From Playlist', icon: Zap },
                ].map((seed) => (
                  <button
                    key={seed.type}
                     onClick={() => loadSeeds(seed.type as SeedType)}
                    className="card p-6 text-center hover:border-orcave-primary/50 transition-colors"
                  >
                    <seed.icon className="w-10 h-10 text-orcave-primary mx-auto mb-2" />
                    <p className="font-medium">{seed.label}</p>
                    <p className="text-sm text-orcave-muted">Browse to select</p>
                  </button>
                ))}
              </div>
            )}
            {!selectedSeed && seedCandidates.length > 0 && (
              <div className="card p-4">
                <h3 className="font-medium mb-2">Select {seedType} seed</h3>
                {seedCandidates.map((item) => (
                  <button key={item.id} onClick={() => selectSeed(item)} className="block w-full text-left p-2 hover:bg-orcave-surfaceElevated">
                    {item.title}
                  </button>
                ))}
              </div>
            )}
          </section>
        </div>
      )}
    </div>
  )
}
