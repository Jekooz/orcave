import { Routes, Route, Navigate } from 'react-router-dom'
import { Sidebar } from '@/components/Sidebar'
import { Header } from '@/components/Header'
import { AuthPage } from '@/pages/AuthPage'
import { CataloguePage } from '@/pages/CataloguePage'
import { LibraryPage } from '@/pages/LibraryPage'
import { PlaybackPage } from '@/pages/PlaybackPage'
import { QueuePage } from '@/pages/QueuePage'
import { RecommendationsPage } from '@/pages/RecommendationsPage'
import { SettingsPage } from '@/pages/SettingsPage'
import { DiagnosticsPage } from '@/pages/DiagnosticsPage'
import { useAuth } from '@/hooks/useAuth'
import { useAuthStore } from '@/lib/stores'

function Layout({ children }: { children: React.ReactNode }) {
  return (
    <div className="min-h-screen bg-orcave-bg text-orcave-text font-sans">
      <Header />
      <div className="flex">
        <Sidebar />
        <main className="flex-1 p-6 overflow-auto">
          {children}
        </main>
      </div>
    </div>
  )
}

function ProtectedRoute({ children }: { children: React.ReactNode }) {
  const { loading } = useAuthStore()

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="animate-pulse-soft text-orcave-primary">Loading...</div>
      </div>
    )
  }

  // YouTube Music is not a supported provider in this alpha: the connection
  // stays disabled (auth state never becomes 'authenticated'), but the local
  // library, queue and playback pages remain usable without it.
  return <>{children}</>
}

export default function App() {
  useAuth()
  return (
    <Routes>
      <Route path="/auth" element={<AuthPage />} />
      <Route
        path="/*"
        element={
          <Layout>
            <ProtectedRoute>
              <Routes>
                <Route path="/" element={<Navigate to="/catalogue" replace />} />
                <Route path="/catalogue" element={<CataloguePage />} />
                <Route path="/library" element={<LibraryPage />} />
                <Route path="/playback" element={<PlaybackPage />} />
                <Route path="/queue" element={<QueuePage />} />
                <Route path="/recommendations" element={<RecommendationsPage />} />
                <Route path="/settings" element={<SettingsPage />} />
                <Route path="/diagnostics" element={<DiagnosticsPage />} />
              </Routes>
            </ProtectedRoute>
          </Layout>
        }
      />
    </Routes>
  )
}
