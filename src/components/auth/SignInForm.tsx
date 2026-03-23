import { useState, useEffect } from 'react'
import { useSignIn } from '@clerk/clerk-react'
import { invoke } from '@tauri-apps/api/core'
import { WebviewWindow } from '@tauri-apps/api/webviewWindow'
import { listen } from '@tauri-apps/api/event'
import { TAGLINES } from '../../lib/constants'

export function SignInForm() {
  const { isLoaded, signIn, setActive } = useSignIn()
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const [showPassword, setShowPassword] = useState(false)
  const [isLoading, setIsLoading] = useState(false)
  const [isGoogleLoading, setIsGoogleLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [taglineIndex, setTaglineIndex] = useState(0)

  useEffect(() => {
    const interval = setInterval(() => {
      setTaglineIndex(prev => (prev + 1) % TAGLINES.length)
    }, 4000)
    return () => clearInterval(interval)
  }, [])

  const handleSubmit = async (e: React.SyntheticEvent) => {
    e.preventDefault()
    if (!isLoaded || !signIn) return

    setIsLoading(true)
    setError(null)

    try {
      const result = await signIn.create({ identifier: email, password })
      if (result.status === 'complete') {
        await setActive({ session: result.createdSessionId })
      } else {
        setError('Sign in failed. Please check your credentials.')
      }
    } catch (err: unknown) {
      const clerkError = err as { errors?: { longMessage?: string }[] }
      setError(
        clerkError.errors?.[0]?.longMessage ?? 'An error occurred. Try again.'
      )
    } finally {
      setIsLoading(false)
    }
  }

  const handleGoogleSignIn = async () => {
    if (!isLoaded || !signIn) return
    setIsGoogleLoading(true)
    setError(null)
    try {
      // Step 1: Create Clerk sign-in attempt (does NOT redirect the current window)
      const result = await signIn.create({
        strategy: 'oauth_google',
        redirectUrl: window.location.origin + '/sso-callback',
        actionCompleteRedirectUrl: window.location.origin,
        oidcPrompt: 'select_account',
      })

      const oauthUrl =
        result.firstFactorVerification.externalVerificationRedirectURL
      if (!oauthUrl) {
        throw new Error('Failed to get OAuth URL from Clerk')
      }

      // Step 2: Open OAuth URL in a popup via Rust command.
      // The Rust side has an on_navigation guard that blocks Clerk's Account
      // Portal redirect for unregistered users.
      await invoke('open_oauth_popup', { url: oauthUrl.toString() })

      const popup = await WebviewWindow.getByLabel('oauth-popup')

      // Step 3: Listen for completion event from popup's SSOCallback
      const unlistenAuth = await listen('clerk-auth-complete', () => {
        window.location.reload()
      })

      // Blocked sign-up: unregistered user tried to sign in with Google
      const unlistenBlocked = await listen('oauth-signup-blocked', () => {
        setError(
          'No account found. Join the waitlist at astral.now to get access.'
        )
        setIsGoogleLoading(false)
      })

      if (popup) {
        popup.once('tauri://error', () => {
          unlistenAuth()
          unlistenBlocked()
          setIsGoogleLoading(false)
          setError('Failed to open sign-in window')
        })

        // Fires on both user-initiated close AND programmatic close
        popup.once('tauri://destroyed', () => {
          unlistenAuth()
          unlistenBlocked()
          setIsGoogleLoading(false)
        })
      }
    } catch (err: unknown) {
      const clerkError = err as { errors?: { longMessage?: string }[] }
      setError(clerkError.errors?.[0]?.longMessage ?? 'Google sign-in failed.')
      setIsGoogleLoading(false)
    }
  }

  const busy = isLoading || isGoogleLoading

  return (
    <div className="sign-in-page">
      {/* Hero */}
      <div className="hero">
        <img src="/AstralIcon.png" alt="Astral" className="app-icon" />
        <h1>Welcome to Astral</h1>
        <p className="tagline">
          Give agents{' '}
          <span className="hero-word" key={taglineIndex}>
            {TAGLINES[taglineIndex]}
          </span>
          , let them work for you
        </p>
      </div>

      {/* Auth form */}
      <div className="sign-in-fields">
        <p className="auth-account-hint">
          Sign in with the same Astral account you use on the web.
        </p>

        {/* Google */}
        <button
          type="button"
          className="auth-google-btn"
          onClick={handleGoogleSignIn}
          disabled={busy || !isLoaded}
        >
          {isGoogleLoading ? (
            'Redirecting…'
          ) : (
            <>
              <svg className="auth-google-icon" viewBox="0 0 24 24">
                <path
                  d="M22.56 12.25c0-.78-.07-1.53-.2-2.25H12v4.26h5.92c-.26 1.37-1.04 2.53-2.21 3.31v2.77h3.57c2.08-1.92 3.28-4.74 3.28-8.09z"
                  fill="#4285F4"
                />
                <path
                  d="M12 23c2.97 0 5.46-.98 7.28-2.66l-3.57-2.77c-.98.66-2.23 1.06-3.71 1.06-2.86 0-5.29-1.93-6.16-4.53H2.18v2.84C3.99 20.53 7.7 23 12 23z"
                  fill="#34A853"
                />
                <path
                  d="M5.84 14.09c-.22-.66-.35-1.36-.35-2.09s.13-1.43.35-2.09V7.07H2.18C1.43 8.55 1 10.22 1 12s.43 3.45 1.18 4.93l2.85-2.22.81-.62z"
                  fill="#FBBC05"
                />
                <path
                  d="M12 5.38c1.62 0 3.06.56 4.21 1.64l3.15-3.15C17.45 2.09 14.97 1 12 1 7.7 1 3.99 3.47 2.18 7.07l3.66 2.84c.87-2.6 3.3-4.53 6.16-4.53z"
                  fill="#EA4335"
                />
              </svg>
              Sign in with Google
            </>
          )}
        </button>

        {/* Divider */}
        <div className="auth-divider">
          <span />
          <p>or</p>
          <span />
        </div>

        {/* Email + password */}
        <form onSubmit={handleSubmit} className="auth-email-fields">
          <div className="auth-input-wrap">
            <svg
              className="auth-input-icon"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
            >
              <rect x="2" y="4" width="20" height="16" rx="2" />
              <path d="m22 7-8.97 5.7a1.94 1.94 0 0 1-2.06 0L2 7" />
            </svg>
            <input
              type="email"
              value={email}
              onChange={e => setEmail(e.target.value)}
              placeholder="Email"
              required
              disabled={busy}
              autoComplete="email"
            />
          </div>

          <div className="auth-input-wrap">
            <svg
              className="auth-input-icon"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
            >
              <rect x="3" y="11" width="18" height="11" rx="2" ry="2" />
              <path d="M7 11V7a5 5 0 0 1 10 0v4" />
            </svg>
            <input
              type={showPassword ? 'text' : 'password'}
              value={password}
              onChange={e => setPassword(e.target.value)}
              placeholder="Password"
              required
              disabled={busy}
              autoComplete="current-password"
            />
            <button
              type="button"
              className="auth-toggle-pw"
              onClick={() => setShowPassword(v => !v)}
              tabIndex={-1}
              aria-label={showPassword ? 'Hide password' : 'Show password'}
            >
              {showPassword ? (
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                >
                  <path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94" />
                  <path d="M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19" />
                  <line x1="1" y1="1" x2="23" y2="23" />
                </svg>
              ) : (
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                >
                  <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" />
                  <circle cx="12" cy="12" r="3" />
                </svg>
              )}
            </button>
          </div>

          {error && <p className="auth-error">{error}</p>}

          <div id="clerk-captcha" />

          <button
            type="submit"
            className="auth-submit-btn"
            disabled={busy || !isLoaded}
          >
            {isLoading ? 'Signing in…' : 'Sign in'}
          </button>
        </form>
      </div>
    </div>
  )
}
