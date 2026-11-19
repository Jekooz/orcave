import { useAuth } from '@/hooks/useAuth'
import { Loader2, Music, AlertCircle, RefreshCw } from 'lucide-react'
import { Button } from '@/components/ui/Button'

export function AuthPage() {
  const { loading, nativeStatus, actionError, refreshStatus, signOut } = useAuth()

  const subtitle = nativeStatus === 'local_session'
    ? 'Local native session detected. YouTube Music access is not verified.'
    : nativeStatus === 'expired'
      ? 'Local native session expired. YouTube Music access is not verified.'
      : nativeStatus === 'error'
        ? 'Could not verify local native session status.'
        : 'YouTube Music browser connection is not available yet.'

  if (loading) {
    return (
      <div className="flex items-center justify-center h-screen">
        <Loader2 className="w-8 h-8 text-orcave-primary animate-spin" aria-hidden="true" />
        <p className="text-orcave-muted">Checking local session...</p>
      </div>
    )
  }

  return (
    <div className="flex items-center justify-center h-screen p-4">
      <div className="card p-8 max-w-md w-full animate-in">
        <div className="text-center mb-8">
          <Music className="w-12 h-12 text-orcave-primary mx-auto mb-4" aria-hidden="true" />
          <h1 className="text-2xl font-bold">Orcave 2.0 Alpha</h1>
          <p className="text-orcave-muted mt-2" aria-live="polite">{subtitle}</p>
        </div>
        <div className="space-y-4">
          <p className="text-sm text-orcave-muted">
            CometHTM companion installation and browser integration have not been verified.
            Connection is disabled until a supported companion and native provider validation are available.
          </p>
          <p className="text-sm text-orcave-muted">
            Do not install extensions from unknown sources or paste passwords, cookies, tokens, or request headers into Orcave.
            Install only a verified companion from its official publisher when support is announced.
          </p>
          <Button className="w-full" size="lg" disabled>
            Connect to YouTube Music (not available)
          </Button>
          <Button onClick={() => void refreshStatus()} variant="secondary" className="w-full">
            <RefreshCw className="w-4 h-4" aria-hidden="true" /> Check local session status
          </Button>
          {nativeStatus === 'local_session' && (
            <Button onClick={() => void signOut()} variant="danger" className="w-full">
              Disconnect local session
            </Button>
          )}
          {actionError && (
            <p role="alert" className="p-3 rounded-lg bg-orcave-error/10 border border-orcave-error/30 text-orcave-error text-sm">
              <AlertCircle className="w-4 h-4 inline-block mr-2" aria-hidden="true" />
              Native session action failed. Try again.
            </p>
          )}
        </div>
      </div>
    </div>
  )
}
