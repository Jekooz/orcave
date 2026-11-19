import { useState, useEffect } from 'react'
import { clsx } from 'clsx'
import { useCatalogue, BROWSE_IDS } from '@/hooks/useCatalogue'
import { Search, Home, Library, Compass, Heart, History, Download, Loader2 } from 'lucide-react'
import { MediaItemCard } from '@/components/MediaItemCard'
import { SectionTitle } from '@/components/SectionTitle'

export function CataloguePage() {
  const { currentBrowse, searchResults, searchLoading, browseLoading, browse, search } = useCatalogue()
  const [error, setError] = useState('')
  useEffect(() => {
    browse(BROWSE_IDS.home).catch((reason) => setError(String(reason)))
  }, [browse])
  const [activeTab, setActiveTab] = useState<'home' | 'library' | 'explore' | 'liked' | 'history' | 'downloads'>('home')
  const [searchQuery, setSearchQuery] = useState('')
  const [searchFilter, setSearchFilter] = useState<'all' | 'tracks' | 'albums' | 'artists' | 'playlists'>('all')

  const tabs = [
    { id: 'home', label: 'Home', icon: Home },
    { id: 'library', label: 'Library', icon: Library },
    { id: 'explore', label: 'Explore', icon: Compass },
    { id: 'liked', label: 'Liked Music', icon: Heart },
    { id: 'history', label: 'History', icon: History },
    { id: 'downloads', label: 'Downloads', icon: Download },
  ] as const

  const handleTabClick = async (tabId: typeof activeTab) => {
    setActiveTab(tabId)
    if (!browseLoading) {
       try {
         await browse(BROWSE_IDS[tabId === 'liked' ? 'liked_music' : tabId])
         setError('')
       } catch (reason) {
         setError(String(reason))
       }
    }
  }

  const handleSearch = async (e: React.FormEvent) => {
    e.preventDefault()
    if (searchQuery.trim()) {
      try {
        await search(searchQuery, searchFilter !== 'all' ? searchFilter : undefined)
        setError('')
      } catch (reason) {
        setError(String(reason))
      }
    }
  }

  return (
    <div className="space-y-6">
      {/* Search Bar */}
      <form onSubmit={handleSearch} className="flex gap-3">
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-5 h-5 text-orcave-muted" />
          <input
            type="search"
            aria-label="Search catalogue"
            name="query"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder="Search songs, artists, albums..."
            className="input pl-10"
          />
        </div>
        <select
          aria-label="Search filter"
          name="filter"
          value={searchFilter}
          onChange={(e) => setSearchFilter(e.target.value as typeof searchFilter)}
          className="input w-40"
        >
          <option value="all">All</option>
          <option value="tracks">Tracks</option>
          <option value="albums">Albums</option>
          <option value="artists">Artists</option>
          <option value="playlists">Playlists</option>
        </select>
        <button type="submit" className="btn-secondary" disabled={searchLoading}>Search</button>
      </form>
      {error && <p role="status">{error}</p>}

      {/* Tab Navigation */}
      <div className="flex gap-1 overflow-x-auto pb-2">
        {tabs.map((tab) => {
          const Icon = tab.icon
          const isActive = activeTab === tab.id
          return (
            <button
              key={tab.id}
              onClick={() => handleTabClick(tab.id)}
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

      {/* Content */}
      {searchResults && searchResults.results.length > 0 && (
        <section className="animate-in">
          <SectionTitle>Search Results for "{searchQuery}"</SectionTitle>
          <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-4">
            {searchResults.results.map((item) => (
              <MediaItemCard key={item.id} item={item} />
            ))}
          </div>
        </section>
      )}

      {currentBrowse && currentBrowse.sections.length > 0 && (
        <>
          {currentBrowse.sections.map((section, sectionIndex) => (
            <section key={sectionIndex} className="animate-in">
              {section.title && <SectionTitle>{section.title}</SectionTitle>}
              {section.items.length > 0 && (
                <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-4">
                  {section.items.map((item) => (
                    <MediaItemCard key={item.id} item={item} />
                  ))}
                </div>
              )}
            </section>
          ))}

        </>
      )}

      {(!currentBrowse || currentBrowse.sections.length === 0) && !searchResults && !browseLoading && (
        <div className="flex items-center justify-center h-64 text-orcave-muted">
          <p>No content available. Try browsing a different section.</p>
        </div>
      )}

      {browseLoading && (
        <div className="flex items-center justify-center h-32">
          <Loader2 className="w-8 h-8 text-orcave-primary animate-spin" />
        </div>
      )}
    </div>
  )
}
