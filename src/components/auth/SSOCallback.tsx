import { useEffect, useRef } from 'react'
import { AuthenticateWithRedirectCallback, useAuth } from '@clerk/clerk-react'
import { emitTo } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'

/**
 * Rendered in the OAuth popup window.
 *
 * AuthenticateWithRedirectCallback processes the OAuth callback.
 * transferable={false} blocks auto sign-up for unknown users —
 * Clerk redirects to signUpUrl (/__blocked) instead.
 *
 * If the URL is /__blocked, we show an error.
 * If sign-in succeeds, we notify the main window and close.
 */
export function OAuthPopupGuard() {
  const { isSignedIn, isLoaded } = useAuth()
  const hasEmitted = useRef(false)

  // Clerk redirects here when transferable={false} blocks a sign-up attempt
  const isBlocked = window.location.pathname === '/__blocked'

  useEffect(() => {
    if (!isLoaded || !isSignedIn || hasEmitted.current) return
    hasEmitted.current = true

    void (async () => {
      await emitTo('main', 'clerk-auth-complete')
      await getCurrentWindow().close()
    })()
  }, [isLoaded, isSignedIn])

  if (isBlocked) {
    return (
      <div
        style={{ padding: 24, fontFamily: 'system-ui', textAlign: 'center' }}
      >
        <p style={{ color: '#c0392b', marginBottom: 12 }}>
          No account found. Join the waitlist at astral.now to get access.
        </p>
        <button
          onClick={() => getCurrentWindow().close()}
          style={{
            padding: '8px 20px',
            borderRadius: 8,
            border: '1px solid #ddd',
            cursor: 'pointer',
          }}
        >
          Close
        </button>
      </div>
    )
  }

  return (
    <div style={{ padding: 24, fontFamily: 'system-ui', textAlign: 'center' }}>
      <AuthenticateWithRedirectCallback
        transferable={false}
        signUpUrl="/__blocked"
      />
      <p>Completing sign-in...</p>
    </div>
  )
}
