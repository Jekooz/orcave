import { NavLink, useLocation } from 'react-router-dom'
import { Search, Music, ListMusic, Zap, Settings, Activity, Folder, User, LogOut } from 'lucide-react'
import { useAuthStore } from '@/lib/stores'
import { useAuth } from '@/hooks/useAuth'
import { clsx } from 'clsx'

const navigation = [
  { name: 'Catalogue', href: '/catalogue', icon: Search },
  { name: 'Library', href: '/library', icon: Folder },
  { name: 'Playback', href: '/playback', icon: Music },
  { name: 'Queue', href: '/queue', icon: ListMusic },
  { name: 'Recommendations', href: '/recommendations', icon: Zap },
  { name: 'Settings', href: '/settings', icon: Settings },
  { name: 'Diagnostics', href: '/diagnostics', icon: Activity },
]

export function Sidebar() {
  const location = useLocation()
  const isAuthenticated = useAuthStore((store) => store.state === 'authenticated')
  const { signOut } = useAuth()

  return (
    <aside className="w-64 bg-orcave-surface border-r border-orcave-border flex flex-col h-screen sticky top-0">
      <div className="p-4 border-b border-orcave-border">
        <h1 className="text-xl font-bold text-orcave-primary flex items-center gap-2">
          <svg className="w-6 h-6" fill="currentColor" viewBox="0 0 24 24">
            <path d="M12 3v10.55c-.59-.34-1.27-.55-2-.55-2.21 0-4 1.79-4 4s1.79 4 4 4 4-1.79 4-4V7h4V3h-6z"/>
          </svg>
          Orcave 2.0
        </h1>
        <p className="text-xs text-orcave-muted mt-1">Alpha Diagnostic UI</p>
      </div>

      <nav className="flex-1 p-4 space-y-1 overflow-y-auto">
        {navigation.map((item) => {
          const Icon = item.icon
          const isActive = location.pathname === item.href || location.pathname.startsWith(item.href + '/')
          return (
            <NavLink
              key={item.name}
              to={item.href}
              className={clsx(
                'sidebar-item',
                isActive && 'sidebar-item-active'
              )}
              title={item.name}
            >
              <Icon className="w-5 h-5 flex-shrink-0" />
              <span>{item.name}</span>
            </NavLink>
          )
        })}
      </nav>

      <div className="p-4 border-t border-orcave-border space-y-2">
        {isAuthenticated ? (
          <button
            onClick={signOut}
            className="w-full sidebar-item text-orcave-error hover:bg-orcave-error/10"
          >
            <LogOut className="w-5 h-5" />
            <span>Sign Out</span>
          </button>
        ) : (
          <span className="sidebar-item text-orcave-muted/60 pointer-events-none select-none">
            <User className="w-5 h-5 opacity-50" />
            <span>YouTube Music: Unavailable</span>
          </span>
        )}

        <div className="pt-2 text-xs text-orcave-muted/50">
          <p>Version 0.2.0-alpha</p>
          <p>Rust/Tokio + Tauri 2</p>
          <p className="text-orcave-error">YouTube Music: connection unavailable</p>
        </div>
      </div>
    </aside>
  )
}
