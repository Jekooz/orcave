import { useAuth } from '@/hooks/useAuth'
import { Loader2, Music, AlertCircle, RefreshCw, Browser, ArrowRight, X, CheckCircle } from 'lucide-react'
import { Button } from '@/components/ui/Button'
import { useState, useEffect } from 'react'

export function AuthPage() {
  const { 
    loading, 
    nativeStatus, 
    actionError, 
    refreshStatus, 
    signOut,
    browserAuth,
    startBrowserAuth,
    checkBrowserAuthStatus,
    finishBrowserAuth,
    cancelBrowserAuth,
    handoffAuth,
  } = useAuth()

  const [showPasteFallback, setShowPasteFallback] = useState(false)
  const [pasteCookies, setPasteCookies] = useState('')
  const [pasteHeaders, setPasteHeaders] = useState('')
  const [browserInfo, setBrowserInfo] = useState<string | null>(null)

  // Load browser info on mount
  useEffect(() => {
    const loadBrowserInfo = async () => {
      try {
        const info = await browserInfo()
        setBrowserInfo(info.browser)
      } catch {
        setBrowserInfo('your browser')
      }
    }
    loadBrowserInfo()
  }, [])

  const subtitle = nativeStatus === 'local_session'
    ? 'Local native session detected. YouTube Music access is not verified.'
    : nativeStatus === 'expired'
      ? 'Local native session expired. YouTube Music access is not verified.'
      : nativeStatus === 'error'
        ? 'Could not verify local native session status.'
        : 'Sign in to YouTube Music to access your library and playlists.'

  if (loading) {
    return (
      <div className="flex items-center justify-center h-screen">
        <Loader2 className="w-8 h-8 text-orcave-primary animate-spin" aria-hidden="true" />
        <p className="text-orcave-muted">Checking session...</p>
      </div>
    )
  }

  const handleStartAuth = async () => {
    try {
      const result = await startBrowserAuth(true)
      if (result.state === 'waiting') {
        // Poll for status
        pollAuthStatus()
      }
    } catch (error) {
      console.error('Failed to start auth:', error)
    }
  }

  const handleFinishAuth = async () => {
    try {
      const result = await finishBrowserAuth()
      if (result.state === 'success') {
        await refreshStatus()
      }
    } catch (error) {
      console.error('Failed to finish auth:', error)
    }
  }

  const handleCancelAuth = async () => {
    try {
      await cancelBrowserAuth()
    } catch (error) {
      console.error('Failed to cancel auth:', error)
    }
  }

  const handlePasteFallback = async () => {
    try {
      const cookies = JSON.parse(pasteCookies || '{}')
      const headers = pasteHeaders ? JSON.parse(pasteHeaders) : undefined
      const result = await handoffAuth(cookies, headers)
      if (result.signed_in) {
        setShowPasteFallback(false)
        setPasteCookies('')
        setPasteHeaders('')
        await refreshStatus()
      }
    } catch (error) {
      console.error('Failed to handoff auth:', error)
    }
  }

  const pollAuthStatus = async () => {
    if (browserAuth.state !== 'waiting') return
    
    try {
      const result = await checkBrowserAuthStatus()
      if (result.state === 'waiting') {
        // Continue polling
        setTimeout(pollAuthStatus, 2000)
      } else if (result.state === 'success') {
        await refreshStatus()
      }
    } catch (error) {
      console.error('Failed to check auth status:', error)
    }
  }

  // Start polling if we're in a waiting state
  useEffect(() => {
    if (browserAuth.state === 'waiting') {
      pollAuthStatus()
    }
  }, [browserAuth.state])

  return (
    <div className="flex items-center justify-center h-screen p-4">
      <div className="card p-8 max-w-md w-full animate-in">
        <div className="text-center mb-8">
          <Music className="w-12 h-12 text-orcave-primary mx-auto mb-4" aria-hidden="true" />
          <h1 className="text-2xl font-bold">Orcave 2.0 Alpha</h1>
          <p className="text-orcave-muted mt-2" aria-live="polite">{subtitle}</p>
        </div>
        
        <div className="space-y-4">
          {!browserAuth.state || browserAuth.state === 'idle' ? (
            <>
              <p className="text-sm text-orcave-muted text-center">
                Sign in using your default browser ({browserInfo || 'detecting...'})
              </p>
              <Button 
                className="w-full" 
                size="lg" 
                onClick={handleStartAuth}
                disabled={loading}
              >
                <Browser className="w-4 h-4 mr-2" aria-hidden="true" />
                Sign in with YouTube Music
              </Button>
              <Button 
                variant="secondary" 
                className="w-full"
                onClick={() => setShowPasteFallback(true)}
              >
                Paste request headers instead
              </Button>
            </>
          ) : browserAuth.state === 'waiting' ? (
            <>
              <div className="text-center py-4">
                <div className="flex items-center justify-center gap-2 mb-4">
                  <Loader2 className="w-6 h-6 text-orcave-primary animate-spin" aria-hidden="true" />
                  <span className="text-orcave-primary font-medium">Waiting for sign-in...</span>
                </div>
                <p className="text-sm text-orcave-muted mb-4">
                  A {browserAuth.browser} window has opened. Sign in to YouTube Music there,
                  then click <strong>"I've signed in"</strong> or close the window.
                </p>
                <div className="space-y-2">
                  <Button 
                    className="w-full" 
                    size="lg"
                    onClick={handleFinishAuth}
                  >
                    <CheckCircle className="w-4 h-4 mr-2" aria-hidden="true" />
                    I've signed in
                  </Button>
                  <Button 
                    variant="secondary" 
                    className="w-full"
                    onClick={handleCancelAuth}
                  >
                    <X className="w-4 h-4 mr-2" aria-hidden="true" />
                    Cancel
                  </Button>
                </div>
              </div>
            </>
          ) : browserAuth.state === 'success' ? (
            <div className="text-center py-4">
              <CheckCircle className="w-12 h-12 text-green-500 mx-auto mb-4" aria-hidden="true" />
              <h3 className="text-lg font-semibold text-green-500">Signed in successfully!</h3>
              <p className="text-orcave-muted mt-2">Redirecting to your library...</p>
            </div>
          ) : browserAuth.state === 'failed' || browserAuth.state === 'error' ? (
            <div className="text-center py-4">
              <AlertCircle className="w-12 h-12 text-orcave-error mx-auto mb-4" aria-hidden="true" />
              <h3 className="text-lg font-semibold text-orcave-error">Sign-in failed</h3>
              <p className="text-orcave-muted mt-2">
                {browserAuth.error || 'An unknown error occurred'}
              </p>
              <Button 
                variant="secondary" 
                className="w-full mt-4"
                onClick={handleStartAuth}
              >
                Try again
              </Button>
            </div>
          ) : browserAuth.state === 'cancelled' ? (
            <div className="text-center py-4">
              <X className="w-12 h-12 text-orcave-muted mx-auto mb-4" aria-hidden="true" />
              <h3 className="text-lg font-semibold">Sign-in cancelled</h3>
              <Button 
                variant="secondary" 
                className="w-full mt-4"
                onClick={handleStartAuth}
              >
                Try again
              </Button>
            </div>
          ) : null}

          {showPasteFallback && (
            <div className="border-t pt-4 space-y-4">
              <h4 className="font-medium">Paste request headers (fallback)</h4>
              <p className="text-sm text-orcave-muted">
                1. Open <a href="https://music.youtube.com" target="_blank" rel="noopener noreferrer" className="underline">music.youtube.com</a> in your browser<br />
                2. Play any video, open DevTools (F12) → Network tab<br />
                3. Filter by "browse", click a POST request<br />
                4. Copy the Request Headers (right-click → Copy → Copy request headers)<br />
                5. Paste below
              </p>
              <div className="space-y-2">
                <textarea
                  className="w-full p-2 rounded border bg-orcave-surface text-sm font-mono"
                  rows={6}
                  placeholder='Paste request headers here (JSON format: {"cookie": "...", "authorization": "...", "user-agent": "..."})'
                  value={pasteCookies}
                  onChange={(e) => setPasteCookies(e.target.value)}
                />
                <textarea
                  className="w-full p-2 rounded border bg-orcave-surface text-sm font-mono"
                  rows={4}
                  placeholder='Optional: additional headers as JSON'
                  value={pasteHeaders}
                  onChange={(e) => setPasteHeaders(e.target.value)}
                />
                <div className="flex gap-2">
                  <Button className="flex-1" onClick={handlePasteFallback}>
                    <ArrowRight className="w-4 h-4 mr-2" aria-hidden="true" />
                    Sign in with headers
                  </Button>
                  <Button variant="secondary" className="flex-1" onClick={() => setShowPasteFallback(false)}>
                    Cancel
                  </Button>
                </div>
              </div>
            </div>
          )}

          <Button 
            onClick={() => void refreshStatus()} 
            variant="secondary" 
            className="w-full"
          >
            <RefreshCw className="w-4 h-4" aria-hidden="true" /> Check session status
          </Button>
          
          {nativeStatus === 'local_session' && (
            <Button 
              onClick={() => void signOut()} 
              variant="danger" 
              className="w-full"
            >
              Disconnect local session
            </Button>
          )}
          
          {actionError && (
            <p role="alert" className="p-3 rounded-lg bg-orcave-error/10 border border-orcave-error/30 text-orcave-error text-sm">
              <AlertCircle className="w-4 h-4 inline-block mr-2" aria-hidden="true" />
              Action failed. Try again.
            </p>
          )}
        </div>
      </div>
    </div>
  )
}