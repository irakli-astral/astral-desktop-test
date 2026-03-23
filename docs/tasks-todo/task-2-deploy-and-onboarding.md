# Task: Deploy Everything + User Onboarding

## Status of All 5 Projects

Quick legend: ✅ Done · ⚠️ Partial · ❌ Not started

---

## 1. `astral-desktop` (Tauri)

### What's done

- ✅ Clerk auth (email + Google OAuth)
- ✅ Custom sign-in form (no Clerk UI)
- ✅ Auto-connect on launch with localStorage cache
- ✅ User avatar / name / sign-out
- ✅ Close-to-tray (tunnel stays alive when window closed)
- ✅ Tray icon: left-click shows window, right-click shows Quit
- ✅ GitHub Actions release workflow (`.github/workflows/release.yml`) — builds DMG, MSI, AppImage

### What's missing

| #   | Task                                                                                          | Notes                                                                                                                                                              |
| --- | --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| D1  | Switch to **production** Clerk publishable key                                                | Current key is `pk_test_...`. Create prod key in Clerk dashboard and bake into release build.                                                                      |
| D2  | Register **Google OAuth redirect URLs** in Clerk dashboard                                    | Add `http://localhost:1420` (dev) and `tauri://localhost` (prod) under Allowed Redirect URLs. Without this Google sign-in silently fails on prod builds.           |
| D3  | Set `relayUrl` to production relay (`wss://relay.astral.com/tunnel`)                          | Right now hardcoded via localStorage. Will be auto-fetched from `/api/desktop/config` once Part 2 is done.                                                         |
| D4  | **Reconnect on network change**                                                               | App doesn't re-connect after sleep/wake or network switch. Listen for Tauri OS events or implement keepalive ping. Low priority for v1 — user can click Reconnect. |
| D5  | Add `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` to GitHub repo secrets | Required by CI workflow for update signing. Generate once: `pnpm tauri signer generate`.                                                                           |
| D6  | Add Apple notarization secrets to GitHub (see macOS section below)                            | Required for DMG to install without warnings.                                                                                                                      |
| D7  | Replace `app.astral.com/api/desktop/config` REST call with actual endpoint                    | Endpoint doesn't exist yet — see `astral/` tasks below.                                                                                                            |

---

## 2. `astral-relay` (Rust)

### What's done

- ✅ WebSocket tunnel acceptor (`/tunnel`, port 3100)
- ✅ HTTP CONNECT proxy (port 8080) with Basic auth
- ✅ Bearer token auth for desktop → relay WS connection
- ✅ yamux stream multiplexing
- ✅ Domain/port allowlist
- ✅ Internal status API (`/api/tunnel/status`, requires `X-Internal-Key`)
- ✅ Dockerfile (multi-stage, final image ~20MB)
- ✅ Tests (27 unit + integration)

### What's missing

| #   | Task                                  | Notes                                                                                                                                                                                                                                         |
| --- | ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R1  | **Deploy to cloud**                   | See deployment steps below. Needs a server with ports 3100 + 8080 exposed.                                                                                                                                                                    |
| R2  | **Domain + TLS**                      | Set `wss://relay.astral.com` (WebSocket must be wss:// in prod). Use nginx reverse proxy or Caddy in front.                                                                                                                                   |
| R3  | **Dynamic user key lookup** (Phase 2) | Right now `USER_KEYS` is a static env var loaded at startup (`user_id:api_key` pairs). For v1 this is fine — pre-load known keys. For scale, relay should query Supabase `desktop_tunnel_tokens` table on each connection instead of env var. |
| R4  | **Production env vars**               | Set `USER_KEYS`, `RELAY_INTERNAL_KEY`, optionally restrict `ALLOWED_DOMAINS`.                                                                                                                                                                 |
| R5  | **Update `last_connected_at`**        | Relay should call Supabase to update `desktop_tunnel_tokens.last_connected_at` on WS connect/disconnect. Not strictly required for v1.                                                                                                        |

### Relay Deploy Steps

#### Option A — Docker on EC2 / Hetzner / DigitalOcean (recommended for v1)

```bash
# 1. Build image
docker build -t astral-relay .

# 2. Push to registry (e.g. ECR, GHCR, Docker Hub)
docker tag astral-relay ghcr.io/youorg/astral-relay:latest
docker push ghcr.io/youorg/astral-relay:latest

# 3. On the server, create .env:
cat > .env << EOF
RELAY_PORT=3100
PROXY_PORT=8080
RELAY_INTERNAL_KEY=<random-secret>
USER_KEYS=user_clerk_id1:api_key1,user_clerk_id2:api_key2
ALLOWED_DOMAINS=*
ALLOWED_PORTS=*
RUST_LOG=info
EOF

# 4. Run
docker run -d --name astral-relay \
  --env-file .env \
  -p 3100:3100 \
  -p 8080:8080 \
  ghcr.io/youorg/astral-relay:latest
```

#### Option B — Fly.io (easiest, auto TLS, global)

```bash
# fly.toml already needed — create one:
cat > fly.toml << EOF
app = "astral-relay"
primary_region = "iad"

[build]
  dockerfile = "Dockerfile"

[[services]]
  internal_port = 3100
  protocol = "tcp"
  [[services.ports]]
    port = 443
    handlers = ["tls", "http"]
  [[services.ports]]
    port = 3100

[[services]]
  internal_port = 8080
  protocol = "tcp"
  [[services.ports]]
    port = 8080
EOF

fly launch --no-deploy
fly secrets set RELAY_INTERNAL_KEY=<secret> USER_KEYS=user_123:test_key ALLOWED_PORTS=*
fly deploy
```

#### Nginx reverse proxy for TLS (if self-hosting)

```nginx
# /etc/nginx/sites-available/relay.astral.com
server {
    listen 443 ssl;
    server_name relay.astral.com;

    ssl_certificate     /etc/letsencrypt/live/relay.astral.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/relay.astral.com/privkey.pem;

    # WebSocket tunnel
    location /tunnel {
        proxy_pass http://localhost:3100;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 86400;
    }

    # Internal API
    location /api/ {
        proxy_pass http://localhost:3100;
    }
}

# HTTP CONNECT proxy stays on raw TCP port 8080 — nginx can't proxy CONNECT.
# Must expose port 8080 directly on the server (no nginx for that port).
```

> **Note**: HTTP CONNECT (port 8080) cannot go through nginx — nginx doesn't support CONNECT method proxying. Port 8080 must be exposed directly from the Docker container to the internet.

---

## 3. `astral` (Next.js)

### What's done

- ✅ Clerk auth, workspace system, all existing features
- ✅ `hyper_browser_profiles` table with `proxy` field

### What's missing

| #   | Task                                                  | Notes                                                                                                                |
| --- | ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| A1  | **Supabase migration**: `desktop_tunnel_tokens` table | See schema below                                                                                                     |
| A2  | **`/api/desktop/config` route**                       | Next.js route handler (not tRPC — desktop uses plain `fetch`)                                                        |
| A3  | **`RELAY_URL` env var**                               | Add to `.env` and Vercel/hosting env: `RELAY_URL=wss://relay.astral.com/tunnel`                                      |
| A4  | **Update HyperBrowser proxy URL** (Phase 2)           | When user connects desktop, update their `hyper_browser_profiles.proxy` to `http://{api_key}:@relay.astral.com:8080` |

### DB Migration (A1)

```sql
-- astral/supabase/migrations/YYYYMMDD_desktop_tunnel_tokens.sql
CREATE TABLE desktop_tunnel_tokens (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id           text NOT NULL,
  api_key           text NOT NULL UNIQUE DEFAULT gen_random_uuid()::text,
  relay_url         text NOT NULL DEFAULT 'wss://relay.astral.com/tunnel',
  is_active         boolean DEFAULT true,
  last_connected_at timestamptz,
  created_at        timestamptz DEFAULT now(),
  updated_at        timestamptz DEFAULT now()
);

-- One active token per user
CREATE UNIQUE INDEX desktop_tunnel_tokens_active_user
  ON desktop_tunnel_tokens(user_id) WHERE is_active = true;

-- Service role bypasses RLS for relay validation
ALTER TABLE desktop_tunnel_tokens ENABLE ROW LEVEL SECURITY;
```

### API Route (A2)

```ts
// astral/src/app/api/desktop/config/route.ts
import { auth } from '@clerk/nextjs/server'
import { NextResponse } from 'next/server'
import { createClient } from '@supabase/supabase-js'

const supabase = createClient(
  process.env.SUPABASE_URL!,
  process.env.SUPABASE_SECRET_KEY!
)

export async function GET() {
  const { userId } = await auth()
  if (!userId)
    return NextResponse.json({ error: 'Unauthorized' }, { status: 401 })

  // Find or create token for this user
  let { data } = await supabase
    .from('desktop_tunnel_tokens')
    .select('api_key, relay_url')
    .eq('user_id', userId)
    .eq('is_active', true)
    .single()

  if (!data) {
    const { data: newToken } = await supabase
      .from('desktop_tunnel_tokens')
      .insert({ user_id: userId, relay_url: process.env.RELAY_URL })
      .select('api_key, relay_url')
      .single()
    data = newToken
  }

  return NextResponse.json({
    api_key: data!.api_key,
    relay_url: data!.relay_url,
  })
}
```

---

## 4. `astral-automations-nest` (NestJS)

### What's done

- ✅ HyperBrowser profile creation and session management
- ✅ Proxy configured via `proxy_metadata` table (static IPs from vault)

### What's missing

| #   | Task                                                                        | Notes                                                                                                                                                                                            |
| --- | --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| N1  | **Use relay proxy URL** for sessions when user has active desktop (Phase 2) | In `startSessionByAccount`: check `desktop_tunnel_tokens` for user first. If connected → use relay proxy `http://{api_key}:@relay.astral.com:8080`. Fall back to `proxy_metadata` if no desktop. |
| N2  | No blockers for v1                                                          | Current proxy_metadata flow still works. Desktop tunnel is additive.                                                                                                                             |

---

## 5. `AstralGPT` (Python FastAPI)

### What's missing

| #   | Task                                    | Notes                                                                                 |
| --- | --------------------------------------- | ------------------------------------------------------------------------------------- |
| G1  | No changes needed for v1 desktop tunnel | Agents call `startSessionByAccount` in automations-nest, which will be updated in N1. |

---

## macOS DMG Distribution

### What users experience without proper signing

| Signing type                    | User experience                                                              |
| ------------------------------- | ---------------------------------------------------------------------------- |
| No signing                      | macOS blocks the app entirely — "damaged and can't be opened"                |
| Ad-hoc (`signingIdentity: "-"`) | Warning dialog: "unverified developer". Users must right-click → Open (once) |
| Developer ID signed             | Clean install, no warnings, but only if also notarized                       |
| Developer ID signed + notarized | Seamless: double-click DMG → drag to Applications → run                      |

**Current state**: `signingIdentity: "-"` (ad-hoc) — users will see a warning but can still install.

### To go fully seamless (required for smooth user onboarding)

**Cost**: Apple Developer Program — $99/year
**Enrollment**: [developer.apple.com/programs/enroll](https://developer.apple.com/programs/enroll)
Takes 24–48 hours to approve.

**What you get**:

- "Developer ID Application" certificate (for distributing outside App Store)
- Access to notarization tool (appstore connect)
- Valid for 5 years per certificate

### Generating a DMG locally

```bash
cd astral-desktop
pnpm tauri build -- --bundles dmg
# Output: src-tauri/target/release/bundle/dmg/Astral_x.y.z_aarch64.dmg
```

For cross-platform (from CI):

```bash
pnpm tauri build -- --bundles dmg,app    # macOS
pnpm tauri build -- --bundles msi        # Windows
pnpm tauri build -- --bundles appimage   # Linux
```

### CI/CD Release (GitHub Actions)

The workflow at `.github/workflows/release.yml` already builds all three platforms. To activate it:

**Step 1 — Generate updater signing keys** (one time):

```bash
pnpm tauri signer generate -w ~/.tauri/astral.key
```

Outputs a private key and public key. The public key goes in `tauri.conf.json` under `plugins.updater.pubkey`.

**Step 2 — Add GitHub secrets**:

| Secret                               | Value                             |
| ------------------------------------ | --------------------------------- |
| `TAURI_SIGNING_PRIVATE_KEY`          | Contents of `~/.tauri/astral.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password you set (or empty)       |

**Step 3 — Add Apple notarization secrets** (only needed once you have paid account):

| Secret                       | Where to get                                                  |
| ---------------------------- | ------------------------------------------------------------- |
| `APPLE_CERTIFICATE`          | Export Developer ID cert from Keychain as .p12, base64 encode |
| `APPLE_CERTIFICATE_PASSWORD` | Password for the .p12 export                                  |
| `APPLE_SIGNING_IDENTITY`     | `Developer ID Application: Your Name (TEAMID)`                |
| `APPLE_ID`                   | Your Apple ID email                                           |
| `APPLE_PASSWORD`             | App-specific password from appleid.apple.com                  |
| `APPLE_TEAM_ID`              | 10-char team ID from developer.apple.com                      |

**Step 4 — Trigger release**:

```bash
git tag v0.1.0
git push origin v0.1.0
# GitHub Actions builds DMG, MSI, AppImage and creates a GitHub Release
```

### Updating `tauri.conf.json` for production signing

```json
// src-tauri/tauri.conf.json
"bundle": {
  "macOS": {
    "signingIdentity": "Developer ID Application: Astral Inc (XXXXXXXXXX)",
    "notarization": {
      "appleId": "you@astral.com",
      "appleIdPassword": "${APPLE_PASSWORD}",
      "appleTeamId": "XXXXXXXXXX"
    }
  }
}
```

---

## End-to-End Connection Map

```
[Desktop App]
  Clerk sign-in (webview)
    → POST app.astral.com/api/desktop/config  (A2)
    ← { api_key: "abc123", relay_url: "wss://relay.astral.com/tunnel" }
  invoke('connect', { apiKey, relayUrl })
    → WebSocket  wss://relay.astral.com/tunnel
    → Authorization: Bearer abc123
    ← relay validates api_key against USER_KEYS (v1) or desktop_tunnel_tokens (v2)
    ← yamux connection established, tunnel active

[HyperBrowser / Automation]
  proxy = "http://abc123:@relay.astral.com:8080"   (set in hyper_browser_profiles.proxy)
    → HTTP CONNECT relay.astral.com:8080
    → Proxy-Authorization: Basic base64(abc123:)
    ← relay decodes api_key → user_id → opens yamux stream to that desktop
    ← desktop dials linkedin.com:443 from residential IP
    ← bytes pipe bidirectionally

[astral-automations-nest]  (Phase 2)
  startSessionByAccount(accountId)
    → check desktop_tunnel_tokens for user_id
    → if connected: proxy = relay URL
    → else: proxy = proxy_metadata (fallback static IP)
```

---

## Priority Order for v1 Launch

```
1. [R1] Deploy relay to cloud                          ← unblocks everything
2. [R2] Add domain + TLS to relay
3. [A1] Create desktop_tunnel_tokens migration
4. [A2] Add /api/desktop/config route
5. [D1] Swap in production Clerk key
6. [D2] Register Google OAuth redirects in Clerk
7. [D3] Auto-fetch relay URL from API (remove localStorage bypass)
8. [D5] Add TAURI_SIGNING_PRIVATE_KEY to GitHub secrets
9.      Tag v0.1.0 → CI builds DMG
10. (Later) Apple Developer Program for notarized DMG
```

---

## References

- [Tauri v2 macOS Signing](https://v2.tauri.app/distribute/sign/macos/)
- [Tauri v2 DMG](https://v2.tauri.app/distribute/dmg/)
- [Shipping Tauri 2.0 to macOS (DEV Community)](https://dev.to/0xmassi/shipping-a-production-macos-app-with-tauri-20-code-signing-notarization-and-homebrew-mc3)
- [Tauri v2 Code Signing Guide](https://dev.to/tomtomdu73/ship-your-tauri-v2-app-like-a-pro-code-signing-for-macos-and-windows-part-12-3o9n)
