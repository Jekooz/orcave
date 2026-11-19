import { Sun, Moon, Monitor } from 'lucide-react'
import { useAuthStore } from '@/lib/stores'
import { useSettings } from '@/hooks/useSettings'
import { useAuth } from '@/hooks/useAuth'
import { Link } from 'react-router-dom'
import { useState } from 'react'

export function Header() {
  const { user, state } = useAuthStore()
  const { settings, update } = useSettings()
  const { signOut } = useAuth()
  const theme = settings?.theme ?? 'system'
  const [showUserMenu, setShowUserMenu] = useState(false)

  // Sync theme from settings
  const handleThemeChange = async (newTheme: 'system' | 'light' | 'dark') => {
    await update('theme', newTheme)
  }

  return (
    <header className="h-16 bg-orcave-surface/80 backdrop-blur-sm border-b border-orcave-border sticky top-0 z-40">
      <div className="flex items-center justify-between h-full px-4">
        <div className="flex items-center gap-4">
          <h2 className="text-lg font-semibold text-orcave-text hidden sm:block">
            Diagnostic Interface
          </h2>
        </div>

        <div className="flex items-center gap-3">
          {/* Theme Toggle */}
          <div className="flex items-center gap-1 bg-orcave-surfaceElevated rounded-lg p-1">
            <button
              onClick={() => handleThemeChange('light')}
              className={`p-2 rounded transition-colors ${theme === 'light' ? 'bg-orcave-primary text-orcave-bg' : 'text-orcave-muted hover:text-orcave-text'}`}
               aria-label="Light theme"
            >
              <Sun className="w-4 h-4" />
            </button>
            <button
              onClick={() => handleThemeChange('system')}
              className={`p-2 rounded transition-colors ${theme === 'system' ? 'bg-orcave-primary text-orcave-bg' : 'text-orcave-muted hover:text-orcave-text'}`}
               aria-label="System theme"
            >
              <Monitor className="w-4 h-4" />
            </button>
            <button
              onClick={() => handleThemeChange('dark')}
              className={`p-2 rounded transition-colors ${theme === 'dark' ? 'bg-orcave-primary text-orcave-bg' : 'text-orcave-muted hover:text-orcave-text'}`}
               aria-label="Dark theme"
            >
              <Moon className="w-4 h-4" />
            </button>
          </div>

          {/* User Menu */}
          {state === 'authenticated' && user && (
            <div className="relative">
              <button
                 onClick={() => setShowUserMenu(!showUserMenu)}
                 aria-expanded={showUserMenu}
                 aria-label="Account menu"
                className="flex items-center gap-2 p-1 rounded-lg hover:bg-orcave-surfaceElevated transition-colors"
              >
                {user.picture ? (
                  <img src={user.picture} alt={user.name} className="w-8 h-8 rounded-full" />
                ) : (
                  <div className="w-8 h-8 rounded-full bg-orcave-primary flex items-center justify-center text-orcave-bg font-medium">
                    {user.name.charAt(0).toUpperCase()}
                  </div>
                )}
                <span className="hidden md:block font-medium">{user.name}</span>
              </button>

              {showUserMenu && (
                <div className="absolute right-0 mt-2 w-48 card-elevated py-2 shadow-lg animate-in">
                  <div className="px-4 py-2 border-b border-orcave-border">
                    <p className="font-medium text-sm">{user.name}</p>
                    <p className="text-xs text-orcave-muted">{user.email}</p>
                  </div>
                  <Link to="/settings" className="block w-full px-4 py-2 text-left text-sm hover:bg-orcave-surfaceElevated">Settings</Link>
                  <hr className="my-2 border-orcave-border" />
                  <button onClick={signOut} className="w-full px-4 py-2 text-left text-sm text-orcave-error hover:bg-orcave-error/10">Sign Out</button>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </header>
  )
}
