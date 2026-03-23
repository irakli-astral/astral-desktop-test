import { useState, useEffect, useCallback, useRef } from 'react'

import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useAuth, useUser } from '@clerk/clerk-react'
import {
  enable as enableAutostart,
  isEnabled as isAutostartEnabled,
} from '@tauri-apps/plugin-autostart'
import { check } from '@tauri-apps/plugin-updater'
import { relaunch } from '@tauri-apps/plugin-process'
import { ask } from '@tauri-apps/plugin-dialog'
import { SignInForm } from './components/auth/SignInForm'
import { initializeCommandSystem } from './lib/commands'
import { TunnelTitleBar } from './components/titlebar/TunnelTitleBar'
import { TAGLINES } from './lib/constants'
import './App.css'

const API_BASE = import.meta.env.VITE_API_BASE ?? 'https://app.astral.com'
const VERCEL_BYPASS_HEADERS: Record<string, string> = import.meta.env
  .VITE_VERCEL_PROTECTION_BYPASS
  ? {
      'x-vercel-protection-bypass': import.meta.env
        .VITE_VERCEL_PROTECTION_BYPASS,
    }
  : {}

interface TunnelStats {
  bytes_up: number
  bytes_down: number
  active_streams: number
  total_streams: number
}

interface StatusResult {
  connected: boolean
  stats: TunnelStats | null
}

interface IpClassification {
  classification: 'green' | 'yellow' | 'red'
  reason: string
  trust_score: number
  ip: string
  city?: string
  isp?: string
  is_safe: boolean
  matched_label: string | null
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B'
  const k = 1024
  const sizes = ['B', 'KB', 'MB', 'GB']
  const i = Math.floor(Math.log(bytes) / Math.log(k))
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i]}`
}

function AppContent() {
  const { getToken, signOut } = useAuth()
  const { user } = useUser()
  const [connected, setConnected] = useState(false)
  const [connecting, setConnecting] = useState(false)
  const [stats, setStats] = useState<TunnelStats | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [taglineIndex, setTaglineIndex] = useState(0)
  const [classification, setClassification] = useState<IpClassification | null>(
    null
  )
  const [networkSafe, setNetworkSafe] = useState<boolean | null>(null) // null = checking
  const [savingNetwork, setSavingNetwork] = useState(false)

  // Refs for stable event listener callbacks — avoids re-registration on state changes.
  const getTokenRef = useRef(getToken)
  useEffect(() => {
    getTokenRef.current = getToken
  })

  // Set to true when the user explicitly clicks Disconnect or signs out.
  const userDisconnectedRef = useRef(false)
  // Mirror ref — let callbacks read current state without adding it to dependency arrays
  const connectingRef = useRef(false)
  useEffect(() => {
    connectingRef.current = connecting
  })

  // Tagline rotation
  useEffect(() => {
    const interval = setInterval(() => {
      setTaglineIndex(prev => (prev + 1) % TAGLINES.length)
    }, 4000)
    return () => clearInterval(interval)
  }, [])

  const registerDevice = useCallback(async () => {
    const token = await getTokenRef.current()
    if (!token) throw new Error('Session not ready')
    const res = await fetch(`${API_BASE}/api/desktop/register-device`, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${token}`,
        'Content-Type': 'application/json',
        ...VERCEL_BYPASS_HEADERS,
      },
      body: JSON.stringify({ device_name: 'Desktop App' }),
      signal: AbortSignal.timeout(15_000),
    })
    if (!res.ok) throw new Error(`Register device failed: ${res.status}`)
    return (await res.json()) as {
      device_token: string
      relay_jwt: string
      relay_jwt_expires_at: number
      desktop_api_jwt?: string
      desktop_api_jwt_expires_at?: number
      relay_url: string
    }
  }, [])
  // Ref for registerDevice so event listeners can use it without re-registering
  const registerDeviceRef = useRef(registerDevice)
  useEffect(() => {
    registerDeviceRef.current = registerDevice
  })

  // Handle connect result from invoke('connect') / invoke('connect_with_stored_credentials').
  // Simplified: Rust NetworkRuntime classifies before emitting tunnel:connected,
  // so no post-connect classification needed here.
  const handleConnectResult = useCallback(
    (result: { success: boolean; error: string | null }): boolean => {
      if (result.success) {
        // Set state directly — don't rely solely on tunnel:connected event.
        // In production builds, the event may have already fired before this
        // callback runs (same race condition as the original white-screen bug).
        setConnected(true)
        setConnecting(false)
        setError(null)
        return true
      }
      if (result.error) {
        if (
          result.error.includes('Auth expired') ||
          result.error.includes('sign in')
        ) {
          return false // Signal caller to fall through to re-registration
        }
        setError(result.error)
        setConnecting(false)
      }
      return true // Handled (even if error) — don't fall through
    },
    []
  )

  const handleConnect = useCallback(async () => {
    if (connectingRef.current) return
    setError(null)
    setConnecting(true)
    try {
      await invoke('disconnect').catch(() => {
        // ignore — no active tunnel to clean up
      })

      const vercelBypass =
        import.meta.env.VITE_VERCEL_PROTECTION_BYPASS || undefined

      if (vercelBypass) {
        await invoke('set_vercel_bypass', { token: vercelBypass }).catch(() => {
          // Non-critical — Vercel bypass only needed behind protection
        })
      }

      // ---------------------------------------------------------------
      // Startup flow: credentials MUST exist before initial_classify,
      // because classify calls get_api_jwt() which needs api_base_url
      // and a device token for the refresh endpoint.
      //
      // Path A (cold relaunch): hydrate stored credentials → classify → connect
      // Path B (first install / re-auth): registerDevice → store credentials → classify → connect
      // ---------------------------------------------------------------

      const hasStored = await invoke<boolean>('has_stored_credentials')

      if (hasStored) {
        // Path A: Cold relaunch — hydrate credentials first, then classify
        try {
          // hydrate_stored_credentials loads stored creds and sets api_base_url
          // so the runtime has a valid URL for classify/refresh calls.
          await invoke('hydrate_stored_credentials')
        } catch (e) {
          const msg = String(e)
          if (msg.includes('Auth expired') || msg.includes('sign in')) {
            // Stored token family is dead — fall through to Path B
          } else {
            setError(msg)
            setConnecting(false)
            return
          }
        }

        // Now classify (credentials are hydrated, runtime has api_base_url)
        try {
          const isSafe = await invoke<boolean>('initial_classify')
          if (!isSafe) {
            setConnecting(false)
            return
          }
        } catch {
          // Classify failed — fail-closed
          setConnecting(false)
          return
        }

        // Gate is open — connect with stored credentials
        try {
          const result = await invoke<{
            success: boolean
            error: string | null
          }>('connect_with_stored_credentials')
          const handled = handleConnectResult(result)
          if (handled) return
          // Auth expired — fall through to Path B (re-registration)
        } catch (e) {
          const msg = String(e)
          if (msg.includes('Auth expired') || msg.includes('sign in')) {
            // Fall through to re-registration below
          } else {
            setError(msg)
            setConnecting(false)
            return
          }
        }
      }

      // Path B: First install or re-registration needed.
      // registerDevice returns credentials including desktop_api_jwt.
      const config = await registerDevice()
      await invoke('prime_credentials', {
        deviceToken: config.device_token,
        relayJwt: config.relay_jwt,
        relayJwtExpiresAt: config.relay_jwt_expires_at,
        desktopApiJwt: config.desktop_api_jwt,
        desktopApiJwtExpiresAt: config.desktop_api_jwt_expires_at,
        relayUrl: config.relay_url,
        apiBaseUrl: API_BASE,
      })

      try {
        const isSafe = await invoke<boolean>('initial_classify')
        if (!isSafe) {
          setConnecting(false)
          return
        }
      } catch {
        setConnecting(false)
        return
      }

      const result = await invoke<{ success: boolean; error: string | null }>(
        'connect',
        {
          deviceToken: config.device_token,
          relayJwt: config.relay_jwt,
          relayJwtExpiresAt: config.relay_jwt_expires_at,
          desktopApiJwt: config.desktop_api_jwt,
          desktopApiJwtExpiresAt: config.desktop_api_jwt_expires_at,
          relayUrl: config.relay_url,
          apiBaseUrl: API_BASE,
          vercelBypass,
        }
      )
      handleConnectResult(result)
    } catch (e) {
      const msg = String(e)
      if (
        msg.includes('Load failed') ||
        msg.includes('fetch') ||
        msg.includes('network')
      ) {
        setError(
          'Could not reach relay server. Check your connection or try again.'
        )
      } else {
        setError(msg)
      }
      setConnecting(false)
    }
  }, [registerDevice, handleConnectResult])

  // Save current network as home or work — delegates to Rust command.
  const saveNetwork = useCallback(async (label: 'home' | 'work') => {
    setSavingNetwork(true)
    try {
      await invoke('save_network', { label })
    } catch (e) {
      setError(String(e))
    }
    setSavingNetwork(false)
  }, [])

  // Poll status every 5 seconds when connected
  useEffect(() => {
    if (!connected) return
    const interval = setInterval(async () => {
      try {
        const result = await invoke<StatusResult>('get_status')
        if (result.stats) setStats(result.stats)
        if (!result.connected) {
          setConnected(false)
          setStats(null)
        }
      } catch {
        // ignore
      }
    }, 5000)
    return () => clearInterval(interval)
  }, [connected])

  const handleDisconnect = useCallback(async () => {
    userDisconnectedRef.current = true
    try {
      await invoke('disconnect')
      setConnected(false)
      setStats(null)
      setClassification(null)
      setNetworkSafe(null)
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const handleSignOut = useCallback(async () => {
    userDisconnectedRef.current = true
    await invoke('disconnect').catch(() => {
      // ignore — tunnel may not be active
    })
    await invoke('clear_credentials').catch(() => {
      // ignore — credentials may not exist
    })
    await signOut()
  }, [signOut])

  // Listen for Rust NetworkRuntime events + tunnel lifecycle events.
  // Rust owns the control plane: classification, heartbeats, network monitoring,
  // periodic polling, and the fail-closed gate. App.tsx is display-only.
  useEffect(() => {
    const unlisten = Promise.all([
      // Network state events from Rust NetworkRuntime
      listen<{
        classification: string
        reason: string
        ip: string
        city?: string
        isp?: string
        is_safe: boolean
        matched_label?: string
      }>('network:safe', e => {
        setNetworkSafe(true)
        setClassification(e.payload as IpClassification)
      }),
      listen<{ reason: string }>('network:unsafe', () => {
        setNetworkSafe(false)
      }),
      listen('network:classifying', () => {
        setNetworkSafe(null)
      }),
      listen('network:unknown', () => {
        setNetworkSafe(null) // show "checking" — fail-closed, retrying
      }),
      listen('network:auth-expired', () => {
        // Re-auth via Clerk -> registerDevice -> update_credentials
        void (async () => {
          try {
            const token = await getTokenRef.current()
            if (!token) {
              setError('Session expired — please sign in again')
              setConnecting(false)
              return
            }
            const config = await registerDeviceRef.current()
            await invoke('update_credentials', {
              deviceToken: config.device_token,
              relayJwt: config.relay_jwt,
              relayJwtExpiresAt: config.relay_jwt_expires_at,
              desktopApiJwt: config.desktop_api_jwt,
              desktopApiJwtExpiresAt: config.desktop_api_jwt_expires_at,
              relayUrl: config.relay_url,
              apiBaseUrl: API_BASE,
            })
          } catch {
            setError('Session expired — please sign in again')
            setConnecting(false)
          }
        })()
      }),
      // Tunnel lifecycle events (still from tunnel-core event forwarder)
      listen('tunnel:connected', () => {
        setConnected(true)
        setConnecting(false)
        setError(null)
      }),
      listen<{ reason?: string }>('tunnel:disconnected', () => {
        setConnected(false)
        setConnecting(false)
        setStats(null)
      }),
      listen('tunnel:connecting', () => {
        setConnecting(true)
        setError(null)
      }),
      listen<{ attempt: number }>('tunnel:reconnecting', () => {
        setConnecting(true)
      }),
      listen<{ message?: string }>('tunnel:error', event => {
        const msg = event.payload?.message ?? 'Unknown tunnel error'
        const isTransient =
          msg.includes('yamux') ||
          msg.includes('WebSocket') ||
          msg.includes('Connection reset') ||
          msg.includes('connection closed') ||
          msg.includes('broken pipe') ||
          msg.includes('failed to lookup address') ||
          msg.includes('nodename nor servname') ||
          msg.includes('Connection failed') ||
          msg.includes('Connection refused') ||
          msg.includes('Network is unreachable') ||
          msg.includes('timed out')
        if (!isTransient) {
          setError(msg)
        }
      }),
      listen('tunnel:auth-expired', () => {
        setConnected(false)
        void invoke('clear_credentials').then(() => {
          void (async () => {
            try {
              const token = await getTokenRef.current()
              if (!token) {
                setConnecting(false)
                setError('Session expired — please sign in again')
                return
              }
              const config = await registerDeviceRef.current()
              const result = await invoke<{
                success: boolean
                error: string | null
              }>('connect', {
                deviceToken: config.device_token,
                relayJwt: config.relay_jwt,
                relayJwtExpiresAt: config.relay_jwt_expires_at,
                desktopApiJwt: config.desktop_api_jwt,
                desktopApiJwtExpiresAt: config.desktop_api_jwt_expires_at,
                relayUrl: config.relay_url,
                apiBaseUrl: API_BASE,
              })
              if (result.success) {
                setConnected(true)
                setConnecting(false)
                setError(null)
              } else {
                setConnecting(false)
                setError(result.error ?? 'Reconnection failed')
              }
            } catch {
              setConnecting(false)
              setError('Session expired — please sign in again')
            }
          })()
        })
      }),
    ])
    // Auto-connect AFTER all listeners registered.
    // Use .then().catch() to ensure handleConnect fires even if a listener fails.
    unlisten
      .then(() => {
        void handleConnect()
      })
      .catch(err => {
        console.error('Failed to register event listeners:', err)
        // Still try to connect — better to connect without events than hang forever
        void handleConnect()
        void isAutostartEnabled()
          .then(enabled => {
            if (!enabled) {
              void enableAutostart().catch(() => {
                // Non-critical — user can enable manually
              })
            }
          })
          .catch(() => {
            // Plugin not available — ignore
          })
      })
    return () => {
      unlisten.then(fns => fns.forEach(fn => fn()))
    }
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  // Derive status for display
  const isUnsafeDisconnected = !connected && networkSafe === false
  const statusLabel = connecting
    ? 'Connecting...'
    : connected
      ? networkSafe === null
        ? 'Checking network...'
        : networkSafe
          ? 'Tunnel active'
          : 'Automations paused'
      : isUnsafeDisconnected
        ? 'Automations paused'
        : 'Not connected'

  const statusClass = connecting
    ? 'connecting'
    : connected
      ? networkSafe === false
        ? 'connecting'
        : 'connected' // yellow for paused
      : isUnsafeDisconnected
        ? 'connecting' // yellow for unsafe-disconnected
        : 'idle'

  return (
    <div className="app-root">
      <TunnelTitleBar />

      <div className="content">
        {/* Hero */}
        <div className="hero">
          <img
            src="/AstralIcon.png"
            alt="Astral"
            className={`app-icon ${connected && networkSafe ? 'floating' : ''}`}
          />
          <h1>Welcome to Astral</h1>
          <p className="tagline">
            Give agents{' '}
            <span className="hero-word" key={taglineIndex}>
              {TAGLINES[taglineIndex]}
            </span>
            , let them work for you
          </p>
        </div>

        {/* Status pill */}
        <div className={`status-pill ${statusClass}`}>
          <span className="status-dot" />
          <span>{statusLabel}</span>
        </div>

        {/* Safe network indicator — shows which saved network matched, with option to relabel */}
        {connected && networkSafe && classification?.matched_label && (
          <div className="network-label-row">
            <span className="network-label-text">
              {classification.matched_label === 'work' ? 'Work' : 'Home'}{' '}
              network
            </span>
            <button
              className="network-relabel-btn"
              onClick={() =>
                saveNetwork(
                  classification.matched_label === 'work' ? 'home' : 'work'
                )
              }
              disabled={savingNetwork}
            >
              {savingNetwork
                ? 'Saving...'
                : `Change to ${classification.matched_label === 'work' ? 'Home' : 'Work'}`}
            </button>
          </div>
        )}

        {/* Unsafe network warning — shown when IP doesn't match any saved profile.
            Visible even when tunnel is disconnected (kill switch keeps banner up). */}
        {networkSafe === false && (
          <div
            className="classification-banner red"
            role="alert"
            aria-live="assertive"
          >
            <span className="classification-dot" />
            <span className="classification-text">
              Not on a saved network. Automations are paused.
            </span>
            <div className="save-network-buttons">
              <button
                className="save-network-btn"
                onClick={() => saveNetwork('home')}
                disabled={savingNetwork}
              >
                {savingNetwork ? 'Saving...' : 'Set as Home'}
              </button>
              <button
                className="save-network-btn"
                onClick={() => saveNetwork('work')}
                disabled={savingNetwork}
              >
                {savingNetwork ? 'Saving...' : 'Set as Work'}
              </button>
            </div>
          </div>
        )}

        {/* IP classification result — only when safe */}
        {connected && networkSafe && classification && (
          <div
            className={`classification-banner ${classification.classification}`}
            role="status"
            aria-live="polite"
          >
            <span className="classification-dot" />
            <span className="classification-text">
              {classification.classification === 'green'
                ? `Residential — ${classification.isp ?? 'ISP'}`
                : classification.classification === 'yellow'
                  ? `Business network — ${classification.isp ?? 'Unknown'}`
                  : `Warning: ${classification.reason}`}
            </span>
            {classification.city && (
              <span className="classification-city">{classification.city}</span>
            )}
          </div>
        )}

        {/* Stats (when connected and safe) */}
        {connected && stats && (
          <div className="stats-grid">
            <div className="stat">
              <span className="stat-label">Upload</span>
              <span className="stat-value">{formatBytes(stats.bytes_up)}</span>
            </div>
            <div className="stat">
              <span className="stat-label">Download</span>
              <span className="stat-value">
                {formatBytes(stats.bytes_down)}
              </span>
            </div>
            <div className="stat">
              <span className="stat-label">Active</span>
              <span className="stat-value">{stats.active_streams}</span>
            </div>
            <div className="stat">
              <span className="stat-label">Total</span>
              <span className="stat-value">{stats.total_streams}</span>
            </div>
          </div>
        )}

        {/* Error */}
        {error && <div className="error-banner">{error}</div>}

        {/* Action button */}
        <div className="actions">
          {connected ? (
            <button
              className="action-btn disconnect"
              onClick={handleDisconnect}
            >
              Disconnect
            </button>
          ) : (
            <button
              className="action-btn connect"
              onClick={() => {
                userDisconnectedRef.current = false
                void handleConnect()
              }}
              disabled={connecting}
            >
              {connecting ? 'Connecting...' : 'Reconnect'}
            </button>
          )}
        </div>

        {/* User info */}
        <div className="user-row">
          {user?.imageUrl && (
            <img src={user.imageUrl} alt="" className="user-avatar" />
          )}
          <span className="user-name">
            {user?.firstName ?? user?.emailAddresses[0]?.emailAddress ?? ''}
          </span>
          <button className="sign-out-btn" onClick={handleSignOut}>
            Sign out
          </button>
        </div>
      </div>
    </div>
  )
}

function App() {
  const { isSignedIn, isLoaded } = useAuth()
  const [updating, setUpdating] = useState(false)

  useEffect(() => {
    initializeCommandSystem()
  }, [])

  useEffect(() => {
    const timer = setTimeout(async () => {
      try {
        console.log('[updater] checking for updates...')
        const update = await check()
        console.log('[updater] check result:', update)
        if (update) {
          console.log('[updater] update available:', update.version)
          const shouldUpdate = await ask(
            `Update available: ${update.version}\n\nWould you like to download and install it?`,
            { title: 'Update Available', kind: 'info' }
          )
          if (shouldUpdate) {
            setUpdating(true)
            console.log('[updater] downloading...')
            await update.downloadAndInstall()
            console.log('[updater] installed, relaunching...')
            await relaunch()
          }
        } else {
          console.log('[updater] no update available')
        }
      } catch (e) {
        console.error('[updater] error:', e)
        setUpdating(false)
      }
    }, 5000)
    return () => clearTimeout(timer)
  }, [])

  if (updating) {
    return (
      <div className="app-root">
        <TunnelTitleBar />
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', flex: 1 }}>
          Downloading update...
        </div>
      </div>
    )
  }

  if (!isLoaded) {
    return (
      <div className="app-root">
        <TunnelTitleBar />
        <div className="auth-loading">
          <div className="auth-loading-dot" />
        </div>
      </div>
    )
  }

  if (!isSignedIn) {
    return (
      <div className="app-root">
        <TunnelTitleBar />
        <div className="auth-container">
          <SignInForm />
        </div>
      </div>
    )
  }

  return <AppContent />
}

export default App
