# Task: Clerk Auth + Relay Connection Architecture

## Goal

Replace the manual API key / relay URL form with automatic Clerk-based auth.
The user signs in once; the app fetches relay credentials from the Astral API
and connects silently. Cloud automations route traffic through the user's
connected desktop.

---

## Current State (What Exists)

- `App.tsx`: manual `apiKey` + `relayUrl` fields stored in `localStorage`
- `tunnel-core`: WebSocket → relay using `Authorization: Bearer {api_key}`, yamux
  multiplexing — relay opens streams, desktop dials target, pipes bytes
- `commands.rs`: `connect(api_key, relay_url)` → `tunnel_core::start(TunnelConfig)`
- Astral backend: `proxy_metadata` table + Supabase Vault for proxy credentials
- `hyper-browser.ts`: `hyper_browser_profiles` with `proxy` field pointing to
  `http://user:pass@ip:port`
- **No Clerk auth in the desktop app. No relay credentials in DB.**

---

## Target Architecture

```
[Desktop App]
  1. User signs in via Clerk (embedded in Tauri webview)
  2. App calls Astral API → GET relay config (relay_url + api_key)
  3. App auto-connects tunnel: WS → relay, Bearer {api_key}
  4. App stays running in tray

[Relay Server]  wss://relay.astral.com/tunnel
  - Validates Bearer api_key against desktop_tunnel_tokens table
  - Accepts inbound WebSocket from desktop
  - Opens yamux streams for each cloud proxy request

[Cloud Automation]  (HyperBrowser, AstralGPT agents)
  - Configured with proxy: http://api_key@relay.astral.com:8888
  - Relay looks up api_key → user_id → routes stream to that user's desktop
```

---

## Part 1 — Clerk Auth in the Desktop App

### Approach

Use `@clerk/clerk-react` directly inside the Tauri React app. Tauri's frontend
IS a browser context (webview), so Clerk's React SDK works as-is — no deep
links, no browser redirect, no PKCE flow needed.

### Changes needed

**`package.json`**

```
npm install @clerk/clerk-react
```

**`.env` (or Vite config)**

```
VITE_CLERK_PUBLISHABLE_KEY=pk_live_...
```

**`src/main.tsx`**

```tsx
import { ClerkProvider } from '@clerk/clerk-react'
;<ClerkProvider publishableKey={import.meta.env.VITE_CLERK_PUBLISHABLE_KEY}>
  <App />
</ClerkProvider>
```

**`src/App.tsx`** — replace manual form with auth gate:

```tsx
import { SignIn, useAuth, useUser } from '@clerk/clerk-react'

// Unauthenticated: show Clerk's hosted sign-in form
if (!isSignedIn) return <SignIn />

// Authenticated: on mount, fetch relay config and auto-connect
```

### On-connect flow (replaces manual form)

1. `useAuth().getToken()` → Clerk JWT
2. `fetch('https://app.astral.com/api/desktop/config', { Authorization: Bearer {jwt} })`
3. Response: `{ relay_url, api_key }`
4. `invoke('connect', { apiKey: api_key, relayUrl: relay_url })`
5. Store `api_key` + `relay_url` in `localStorage` as cache (for offline/fast reconnect)

### UI changes

- Remove `apiKey` input and `relayUrl` input fields from the UI
- Remove "Advanced" toggle section
- App auto-connects on launch if session is active + cached credentials exist
- Show user avatar / name + "Sign out" option (in title bar or settings)

### Issues to track

- [ ] Where does `VITE_CLERK_PUBLISHABLE_KEY` come from in production builds?
      It needs to be baked in at build time (not runtime env). Tauri doesn't have
      process.env at runtime — only `VITE_*` vars embedded at build time.
- [ ] Token refresh: Clerk tokens expire (~1hr). The app should call `getToken()`
      fresh before each API call (it caches internally and auto-refreshes).
- [ ] Sign-out: should disconnect tunnel first, then call `clerk.signOut()`
- [ ] Session persistence: Clerk stores session in `localStorage` / cookies in
      the webview — persists between app relaunches by default. Verify this works
      correctly in Tauri's webview context.

---

## Part 2 — Astral API: Desktop Config Endpoint

New tRPC procedure in `astral/src/server/api/routers/desktop.ts`:

```ts
export const desktopRouter = createTRPCRouter({
  getConfig: protectedProcedure.query(async ({ ctx }) => {
    const supabase = createSupabaseServerClient()

    // Find or create a tunnel token for this user
    const { data, error } = await supabase
      .from('desktop_tunnel_tokens')
      .select('api_key, relay_url')
      .eq('user_id', ctx.userId)
      .eq('is_active', true)
      .single()

    if (!data) {
      // Create new token
      const { data: newToken } = await supabase
        .from('desktop_tunnel_tokens')
        .insert({ user_id: ctx.userId, relay_url: env.RELAY_URL })
        .select()
        .single()
      return { api_key: newToken.api_key, relay_url: newToken.relay_url }
    }

    return { api_key: data.api_key, relay_url: data.relay_url }
  }),

  // Called by the relay server via service role to update last_connected_at
  heartbeat: protectedProcedure.mutation(async ({ ctx }) => {
    await supabase
      .from('desktop_tunnel_tokens')
      .update({ last_connected_at: new Date().toISOString() })
      .eq('user_id', ctx.userId)
  }),
})
```

This can also be a plain Next.js API route (`/api/desktop/config`) instead of
tRPC if the desktop prefers simple `fetch` over tRPC client setup.

### Issues to track

- [ ] tRPC vs plain REST? Desktop currently uses `invoke` (Rust), not tRPC.
      A plain `fetch` to a Next.js API route is simpler for the desktop.
- [ ] The relay server also needs to hit Astral API to validate tokens.
      Use service-role Supabase client in relay, not via the Next.js API.
- [ ] `env.RELAY_URL` — what is the production relay URL? TBD.
- [ ] Token rotation: should `getConfig` rotate the `api_key` on each call,
      or keep the same key? Rotating = more secure but requires reconnect.
      Keep same key unless explicitly revoked.

---

## Part 3 — DB Schema (Supabase Migration)

```sql
CREATE TABLE desktop_tunnel_tokens (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id      text NOT NULL,          -- Clerk user ID (e.g. user_xxxx)
  api_key      text NOT NULL UNIQUE DEFAULT gen_random_uuid()::text,
  relay_url    text NOT NULL DEFAULT 'wss://relay.astral.com/tunnel',
  is_active    boolean DEFAULT true,
  last_connected_at timestamptz,
  created_at   timestamptz DEFAULT now(),
  updated_at   timestamptz DEFAULT now()
);

-- One active token per user
CREATE UNIQUE INDEX desktop_tunnel_tokens_active_user
  ON desktop_tunnel_tokens(user_id)
  WHERE is_active = true;

-- RLS: users can only read their own token (no need to expose api_key to client)
ALTER TABLE desktop_tunnel_tokens ENABLE ROW LEVEL SECURITY;
-- Service role bypasses RLS for relay validation
```

### Issues to track

- [ ] Migration needs to go in `astral/` repo (Supabase migrations dir)
- [ ] RLS policy: the desktop should NOT be able to read `api_key` via Supabase
      directly (only via the Astral API route). The API route uses service role.
- [ ] `user_id` format: confirm Clerk user IDs (`user_xxxx`) vs workspace IDs.
      Should this be per-user or per-workspace? A workspace may have multiple
      users but only one desktop connection. Likely **per-workspace** not per-user.

---

## Part 4 — Relay Server Authentication

The relay server (not yet built or in a separate repo) needs to:

1. Accept WebSocket connections at `wss://relay.astral.com/tunnel`
2. Read `Authorization: Bearer {api_key}` from the upgrade request
3. Validate `api_key` against `desktop_tunnel_tokens` table (Supabase service role)
4. Update `last_connected_at` on connect
5. Map `api_key → user_id` for routing

For each incoming proxy request from a cloud automation:

1. Parse the target address from the yamux stream header
2. Find the connected desktop for this user's api_key
3. Open a yamux stream to that desktop with the target address
4. Pipe bytes bidirectionally

### Proxy endpoint for automations

Cloud automations configure a proxy URL. Two options:

**Option A — Bearer-token-as-username:**

```
http://api_key:@relay.astral.com:8888
```

Relay parses Basic auth username → api_key → routes to desktop.

**Option B — Per-user subdomain:**

```
http://relay.astral.com:8888
```

Relay uses the connected desktop's api_key from the Bearer header.
But cloud automations don't send Bearer headers...

**Option A is simpler** — embed api_key as Basic auth username in proxy URL.

### Issues to track

- [ ] **Relay server doesn't exist yet** — needs to be built. What language/framework?
      Likely Rust (tokio + yamux already used in tunnel-core). Could be a separate
      binary in a `relay-server/` crate alongside `tunnel-core`.
- [ ] The tunnel-core yamux protocol: relay is SERVER mode, desktop is CLIENT mode.
      Current client.rs uses `Mode::Client`. Relay would use `Mode::Server`.
- [ ] Does the relay expose an HTTP CONNECT proxy (standard) or SOCKS5?
      HTTP CONNECT is more widely supported for browser automation.
- [ ] Connection multiplexing: one desktop connection can serve many concurrent
      proxy requests (that's what yamux is for). But what if desktop disconnects
      mid-stream?
- [ ] How does HyperBrowser pass the proxy URL today? Via the `proxy` field in
      `hyper_browser_profiles`. This field would change from a static residential
      IP to the relay proxy URL.

---

## Part 5 — Auto-connect & Reconnect Logic (Desktop)

The tunnel should connect automatically:

- On app launch if session active + cached credentials exist
- After network reconnect (waking from sleep, switching networks)
- With exponential backoff on failure (don't hammer relay)

```tsx
// App.tsx — auto-connect on mount
useEffect(() => {
  if (!isSignedIn) return

  const cachedApiKey = localStorage.getItem('apiKey')
  const cachedRelayUrl = localStorage.getItem('relayUrl')

  if (cachedApiKey && cachedRelayUrl) {
    // Fast path: use cached creds, refresh in background
    invoke('connect', { apiKey: cachedApiKey, relayUrl: cachedRelayUrl })
  } else {
    // Slow path: fetch from API first
    fetchRelayConfig().then(({ api_key, relay_url }) => {
      invoke('connect', { apiKey: api_key, relayUrl: relay_url })
    })
  }
}, [isSignedIn])
```

### Issues to track

- [ ] Reconnect on network change: listen for Tauri's network events or use a
      ping/keepalive in the tunnel to detect disconnection
- [ ] What does the UI show during auto-connect on launch? Probably the status
      pill going "Connecting..." without any user action required.
- [ ] "Connect" button still needed? Maybe repurpose as "Reconnect" only shown
      when disconnected, or remove entirely if auto-connect handles it.

---

## Part 6 — HyperBrowser / Automation Integration

Once the relay is live, the proxy URL in `hyper_browser_profiles` changes:

**Before (static residential proxy):**

```
http://proxyuser:proxypass@123.45.67.89:8080
```

**After (Astral relay — routes through user's desktop):**

```
http://{api_key}:@relay.astral.com:8888
```

In `hyper-browser.ts` router: when creating a profile for a user who has an
active desktop token, set `proxy` to the relay URL automatically.

### Issues to track

- [ ] Fallback: if user's desktop is not connected, what does the relay return
      to the automation? Error 503? Fall back to a static proxy?
- [ ] The `api_key` in the proxy URL is sensitive — do we need to rotate it
      periodically? If proxy URL is stored in hyper_browser_profiles, rotation
      requires updating all profiles.
- [ ] AstralGPT agents: how do they get the proxy URL when launching browser
      sessions? They call `startSessionByAccount` which looks up `proxy_metadata`.
      New flow: look up user's `desktop_tunnel_tokens` instead (or in addition).

---

## Summary of Work by Repo

### `astral-desktop/` (this repo)

- [ ] Install `@clerk/clerk-react`, add `ClerkProvider`
- [ ] Replace manual API key form with Clerk sign-in + auto-connect
- [ ] Remove `apiKey` / `relayUrl` inputs from UI
- [ ] Add user identity display (avatar, name, sign-out)
- [ ] Auto-connect on launch with cached creds
- [ ] Reconnect on disconnect (backoff)

### `astral/` (Next.js)

- [ ] Add `desktop_tunnel_tokens` Supabase migration
- [ ] Add `desktop.getConfig` API route (or tRPC procedure)
- [ ] Wire `RELAY_URL` env var

### Relay Server (new service — TBD)

- [ ] Design and build relay server
- [ ] yamux SERVER mode, validates Bearer api_key
- [ ] Exposes HTTP CONNECT proxy on `:8888` for automations
- [ ] Updates `last_connected_at` in Supabase on connect/disconnect

### Open Architecture Questions

- [ ] Is the relay a new Rust crate in `astral-desktop/crates/` or a separate repo?
- [ ] Production relay URL?
- [ ] Per-user or per-workspace desktop token?
- [ ] Fallback behavior when desktop is offline?
