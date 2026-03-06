# Community Server - Design Document

A self-hosted, headless server binary that provides aggregation, discovery, search, and trending for the Iroh Social P2P network. Users opt in by registering with a server -- the server never scrapes or indexes without consent.

## Table of Contents

- [Architecture Overview](#architecture-overview)
- [Workspace Structure](#workspace-structure)
- [Registration Protocol](#registration-protocol)
- [Post Ingestion](#post-ingestion)
- [Server Storage](#server-storage)
- [HTTP API](#http-api)
- [Trending Algorithm](#trending-algorithm)
- [Client Integration](#client-integration)
- [Server Configuration](#server-configuration)
- [Federation (Phase 2)](#federation-phase-2)
- [Implementation Roadmap](#implementation-roadmap)

---

## Architecture Overview

```
                         +--------------------+
                         | Community Server   |
                         | (headless binary)  |
                         |                    |
  Users opt-in           | - Iroh node        |   HTTP API
  via signed    -------> | - Gossip listener  | <-------  Clients query for
  registration           | - Sync puller      |           search, trending,
                         | - sqlx (SQLite)    |           discovery
                         |                    |
                         | - axum HTTP server |
                         +--------------------+
                                  |
                          Participates in
                          the P2P network
                          as a first-class
                          Iroh node
```

The server runs its own Iroh endpoint and joins the same gossip topics and sync protocol that regular clients use. It stores an aggregated index of all registered users' posts using sqlx with SQLite and FTS5 full-text search. An axum HTTP API exposes this index for search, trending, user discovery, and aggregated feeds.

Key principle: **the server is an overlay, not a replacement**. The P2P layer remains the foundation. Users who never connect to a server lose nothing. Servers add opt-in social features that require aggregation (search, trending, discovery).

---

## Workspace Structure

The repo is already a Cargo workspace. The shared types crate (`crates/iroh-social-types/`) exists and is used by the Tauri app. The server crate is the new addition.

```
iroh-social/
  Cargo.toml                      # Workspace root (existing)
  crates/
    iroh-social-types/            # Shared types and protocol definitions (existing)
      Cargo.toml
      src/
        lib.rs
        types.rs                  # Post, Profile, MediaAttachment, Interaction, FollowEntry, FollowerEntry
        protocol.rs               # GossipMessage, SyncRequest/SyncSummary/SyncFrame (incl. DeviceAnnouncements), SYNC_ALPN (v4), user_feed_topic()
        signing.rs                # sign_post(), verify_post_signature(), sign_interaction(), verify_interaction_signature(), sign_profile(), verify_profile_signature(), sign/verify delete messages
        validation.rs             # validate_post(), validate_interaction(), validate_profile(), constants
        dm.rs                     # DmHandshake, EncryptedEnvelope, DirectMessage, DM_ALPN, etc.
        devices.rs                # DeviceCertificate, LinkedDevicesAnnouncement, DeviceRevocation, etc. (added by linked-devices feature)
        registration.rs           # RegistrationPayload, RegistrationRequest, sign_registration(), verify_registration_signature()
    iroh-social-server/           # Server binary (new)
      Cargo.toml
      migrations/                 # sqlx migrations (SQLite)
      src/
        main.rs                   # CLI entry, config loading, startup
        config.rs                 # TOML config parsing
        node.rs                   # Iroh endpoint, gossip, sync setup
        storage.rs                # sqlx storage (SQLite)
        ingestion.rs              # Gossip subscriber + sync scheduler
        trending.rs               # Trending computation
        api/
          mod.rs                  # axum Router assembly
          server_info.rs          # GET /api/v1/info
          auth.rs                 # POST/DELETE /api/v1/register
          users.rs                # GET /api/v1/users, search, profile
          posts.rs                # GET /api/v1/posts/search
          feed.rs                 # GET /api/v1/feed
          trending.rs             # GET /api/v1/trending
  src-tauri/                      # Existing Tauri app (uses shared crate)
  src/                            # Existing Svelte frontend
```

### Shared crate status

The types crate already contains all shared types, protocols, signing, and validation logic. The Tauri app already imports from `iroh-social-types`. No further extraction is needed -- the server crate simply adds `iroh-social-types` as a dependency.

When the linked-devices feature lands, the types crate gains `devices.rs` with `DeviceCertificate`, `LinkedDevicesAnnouncement`, `DeviceRevocation`, etc. The `GossipMessage` enum gains `LinkedDevices` and `DeviceRevocation` variants, and the existing `DeletePost` and `DeleteInteraction` variants gain `device_pubkey` and `signature` fields for verified deletion. The server must handle all of these (see [Device-Aware Verification](#device-aware-verification)).

**Note**: The shared crate's `signing.rs` utility functions `signature_to_hex()` and `hex_to_signature()` are made `pub` as part of the linked-devices work, since they are needed by `devices.rs`, `registration.rs`, and any other module that performs Ed25519 signature hex encoding.

---

## Registration Protocol

### Design

Single-step signed registration over HTTP. The user signs a payload with their identity key (ed25519) to prove identity. No challenge-response needed -- the payload includes server URL and timestamp to prevent replay.

**Linked-devices note**: Registration requires the identity key signature, so only the primary device can register/unregister. Secondary devices cannot register independently -- they inherit the registration via their identity's primary device. The server ingests posts from all authorized devices of a registered identity (verified via cached device certificates).

### Registration flow

1. User constructs a `RegistrationPayload`:
   ```
   { pubkey, server_url, timestamp, action: None }
   ```
2. Serializes it using canonical JSON (`serde_json::to_vec(&serde_json::json!({...}))`) -- the same pattern used for signing posts, interactions, and profiles throughout the codebase.
3. Signs the bytes with their ed25519 identity key using `sign_registration()` from the shared crate.
4. POSTs a `RegistrationRequest` to the server:
   ```
   { pubkey, server_url, timestamp, action: null, signature }
   ```
5. Server verifies using `verify_registration_signature()` from the shared crate:
   - Timestamp within 5 minutes of server time
   - `server_url` matches the server's own URL
   - Signature is valid for the pubkey over the reconstructed payload bytes
6. Server stores a registration record and begins ingesting the user's posts.

### Unregistration

Same mechanism with `action: Some("unregister")`: user constructs a `RegistrationPayload` with `action: Some("unregister".to_string())`, signs it with `sign_registration()`, and sends to `DELETE /api/v1/register`. Server verifies with `verify_registration_signature()`, stops ingesting posts, and marks user inactive.

### Data types

`RegistrationPayload` and `RegistrationRequest` go in the shared crate (`iroh-social-types/src/registration.rs`) since the client needs them to sign and send registration requests. `Registration` is server-only.

```rust
// In iroh-social-types/src/registration.rs (shared)
// Uses pub signature_to_hex() and hex_to_signature() from signing.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationPayload {
    pub pubkey: String,
    pub server_url: String,
    pub timestamp: u64,
    /// None for registration, Some("unregister") for unregistration.
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationRequest {
    pub pubkey: String,
    pub server_url: String,
    pub timestamp: u64,
    pub action: Option<String>,
    pub signature: String,  // hex-encoded ed25519 signature
}

// Canonical signing bytes (same pattern as posts/interactions)
fn registration_signing_bytes(pubkey: &str, server_url: &str, timestamp: u64, action: &Option<String>) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "pubkey": pubkey,
        "server_url": server_url,
        "timestamp": timestamp,
        "action": action,
    }))
    .expect("json serialization should not fail")
}

pub fn sign_registration(payload: &RegistrationPayload, secret_key: &SecretKey) -> String {
    let bytes = registration_signing_bytes(&payload.pubkey, &payload.server_url, payload.timestamp, &payload.action);
    let sig = secret_key.sign(&bytes);
    signature_to_hex(&sig)
}

pub fn verify_registration_signature(request: &RegistrationRequest) -> Result<(), String> {
    let sig = hex_to_signature(&request.signature)?;
    let pubkey: PublicKey = request.pubkey
        .parse()
        .map_err(|e| format!("invalid pubkey: {e}"))?;
    let bytes = registration_signing_bytes(&request.pubkey, &request.server_url, request.timestamp, &request.action);
    pubkey
        .verify(&bytes, &sig)
        .map_err(|_| "registration signature verification failed".to_string())
}

// Server-only
struct Registration {
    pubkey: String,
    registered_at: u64,
    last_seen: u64,
    display_name: Option<String>,
    bio: Option<String>,
    avatar_hash: Option<String>,
    is_active: bool,
}
```

---

## Post Ingestion

### Dual-mode: gossip + sync

Both mechanisms are needed for completeness:

**Gossip (real-time):** When a user registers, the server subscribes to their gossip topic (`user_feed_topic(pubkey)`). This is the same subscription pattern used by the Tauri client in `gossip.rs`. The server receives `NewPost`, `DeletePost` (signed), `ProfileUpdate`, `NewInteraction`, `DeleteInteraction` (signed), `LinkedDevices`, and `DeviceRevocation` messages in real time. Delete messages include `device_pubkey` and `signature` fields and must be verified before processing.

**Sync (historical catch-up):** Uses the same `SYNC_ALPN` (`b"iroh-social/sync/4"`) protocol and shared types (`SyncRequest`, `SyncSummary`, `SyncFrame`) from the types crate. The server implements its own sync client (it cannot reuse the Tauri-specific code directly, but the protocol is identical). `SyncFrame` includes a `DeviceAnnouncements` variant for exchanging cached `LinkedDevicesAnnouncement` data during sync -- this is critical for the server to obtain device certificates it may have missed via gossip (e.g., if the server started after the announcement was published). **Frame ordering**: `DeviceAnnouncements` frames are sent before `Posts` and `Interactions` frames, so the server has certificates cached before it needs to verify device-signed content. The `SyncSummary` also includes the user's `Profile`, which must be verified via `verify_profile_signature()` using the requested author as `expected_identity` before updating the registrations table. Triggered on:

- Server startup (sync all registered users)
- New user registration (sync their history immediately)
- Periodic catch-up every 15 minutes for users whose last gossip was >30 min ago

### Architecture

```
IngestionManager
  |
  +-- GossipSubscriber (per registered user)
  |     Subscribes to user_feed_topic(pubkey)
  |     Processes all GossipMessage variants:
  |       NewPost, DeletePost, ProfileUpdate,
  |       NewInteraction, DeleteInteraction,
  |       LinkedDevices, DeviceRevocation
  |     Writes to sqlx database
  |
  +-- SyncScheduler
        On startup: sync all registered users
        Every 15 min: catch-up sync for stale users
        On registration: immediate history pull
        Bounded concurrency via semaphore (max 10)
        Fallback: if primary device is offline, try
          secondary devices from cached announcement
```

### Validation

Same checks as the Tauri client:

- `validate_post()` (content length, media count, timestamp drift)
- `validate_interaction()` (timestamp drift)
- `validate_profile()` (display name length, bio length)
- Deduplication via `(author, id)` unique constraint
- Signature verification via `verify_post_signature()` / `verify_interaction_signature()` / `verify_profile_signature()` (includes device-aware two-step verification -- see below)

**Per-variant gossip topic owner validation.** Each message received on `user_feed_topic(pubkey)` must be validated against the topic owner:

| Variant | Validation |
|---------|-----------|
| `NewPost(post)` | `post.author` must equal topic owner |
| `DeletePost { author, .. }` | `author` must equal topic owner; verify `signature` against `device_pubkey` (two-step chain) |
| `ProfileUpdate(profile)` | No `author` field -- use topic owner as `expected_identity` for `verify_profile_signature()` |
| `NewInteraction(interaction)` | `interaction.author` must equal topic owner |
| `DeleteInteraction { author, .. }` | `author` must equal topic owner; verify `signature` against `device_pubkey` (two-step chain) |
| `LinkedDevices(announcement)` | `announcement.identity_pubkey` must equal topic owner |
| `DeviceRevocation(revocation)` | `revocation.identity_pubkey` must equal topic owner |

### Device-Aware Verification

When the linked-devices feature is active, posts and interactions may include a `device_pubkey` field indicating a secondary device signed the content. The server must handle this:

1. **Cache device certificates**: When the server receives a `LinkedDevices(LinkedDevicesAnnouncement)` gossip message, it verifies the announcement signature against the identity pubkey, then caches all device certificates in the `peer_device_certificates` table.
2. **Handle revocations**: When the server receives a `DeviceRevocation` gossip message, it verifies the signature, caches the revocation, and removes the device from the certificate cache. Future posts from the revoked device (with timestamps after the revocation) are rejected. Additionally, the server performs **retroactive cleanup**: it scans already-ingested posts and interactions where `device_pubkey == revoked_device_pubkey` and `timestamp > revocation.timestamp`, and deletes those records. This handles the race condition where a revoked device published content between revocation issuance and gossip propagation.
3. **Two-step post/interaction verification**: When `device_pubkey` is present and differs from `author`:
   - Step 1: Verify the content signature against the device's public key.
   - Step 2: Look up the device's certificate in the cache and verify the certificate is valid (signed by the identity key, not expired, not revoked).
   - If no certificate is cached, reject the post and log a warning. The certificate will arrive via gossip or sync and future posts will verify normally.
4. **Primary device posts**: When `device_pubkey` is empty or equals `author`, fall back to direct signature verification (same as current behavior).
5. **Profile verification**: Profile updates now include `device_pubkey` and `signature` fields. The server verifies profile updates using `verify_profile_signature()` with the expected identity (from the gossip topic or from the `SyncRequest.author` when received via sync). Both gossip-received and sync-received profiles must be verified before updating the registrations table.

The shared crate's `verify_post_signature()`, `verify_interaction_signature()`, and `verify_profile_signature()` functions accept a `get_certificate: impl Fn(&str, &str) -> Option<DeviceCertificate>` callback. The server implements this callback by querying its sqlx `peer_device_certificates` table:

```rust
// Server-side certificate lookup for verification callbacks
let get_cert = |identity: &str, device: &str| -> Option<DeviceCertificate> {
    // Query peer_device_certificates table via sqlx
    // Also check device_revocations to reject revoked devices
    storage.get_peer_device_certificate(identity, device)
        .ok()
        .flatten()
        .filter(|_| !storage.is_device_revoked(identity, device).unwrap_or(true))
};
```

---

## Server Storage

### sqlx with SQLite

The server uses **sqlx** with SQLite for storage:

- Async-native database access fits naturally with axum
- Single file, no external service, zero config for self-hosting
- FTS5 for full-text search
- Connection pooling for concurrent HTTP request handling

### Why not rusqlite?

The Tauri client uses rusqlite because it's an embedded single-user desktop app where async provides no benefit. The server is different: axum is async, so sqlx queries compose naturally without `spawn_blocking`, and connection pooling matters for concurrent HTTP handling.

### Dependencies

```toml
[dependencies]
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite", "migrate"] }
```

### Schema

```sql
CREATE TABLE registrations (
    pubkey TEXT PRIMARY KEY,
    registered_at INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    display_name TEXT,
    bio TEXT,
    avatar_hash TEXT,
    is_active BOOLEAN NOT NULL DEFAULT 1
);

CREATE TABLE posts (
    id TEXT NOT NULL,
    author TEXT NOT NULL,
    content TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    media_json TEXT,
    reply_to TEXT,
    reply_to_author TEXT,
    quote_of TEXT,
    quote_of_author TEXT,
    device_pubkey TEXT NOT NULL DEFAULT '',  -- empty or == author means primary device
    signature TEXT NOT NULL DEFAULT '',
    indexed_at INTEGER NOT NULL,
    PRIMARY KEY (author, id),
    FOREIGN KEY (author) REFERENCES registrations(pubkey)
);

CREATE INDEX idx_posts_timestamp ON posts(timestamp DESC);
CREATE INDEX idx_posts_author_timestamp ON posts(author, timestamp DESC);

-- NOTE: The FK constraint means only interactions from registered users are stored.
-- The server subscribes to each registered user's gossip topic, which carries that
-- user's own interactions (e.g., Alice's likes). Interactions from unregistered users
-- (e.g., an unregistered Bob liking Alice's post) arrive on Bob's topic, which the
-- server does not subscribe to. Consequently, like_count and other aggregate endpoints
-- only reflect registered users' activity. This is intentional: the server is an
-- overlay that indexes opted-in users, not a global aggregator.
CREATE TABLE interactions (
    id TEXT NOT NULL,
    author TEXT NOT NULL,
    kind TEXT NOT NULL,          -- 'Like', etc.
    target_post_id TEXT NOT NULL,
    target_author TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    device_pubkey TEXT NOT NULL DEFAULT '',
    signature TEXT NOT NULL DEFAULT '',
    indexed_at INTEGER NOT NULL,
    PRIMARY KEY (author, id),
    FOREIGN KEY (author) REFERENCES registrations(pubkey)
);

CREATE INDEX idx_interactions_target ON interactions(target_author, target_post_id);
CREATE INDEX idx_interactions_author ON interactions(author, timestamp DESC);

-- Full-text search
CREATE VIRTUAL TABLE posts_fts USING fts5(
    content,
    content=posts,
    content_rowid=rowid,
    tokenize='unicode61'
);

-- Keep FTS in sync automatically
CREATE TRIGGER posts_ai AFTER INSERT ON posts BEGIN
    INSERT INTO posts_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER posts_ad AFTER DELETE ON posts BEGIN
    INSERT INTO posts_fts(posts_fts, rowid, content) VALUES('delete', old.rowid, old.content);
END;

CREATE TABLE trending_hashtags (
    tag TEXT PRIMARY KEY,
    post_count INTEGER NOT NULL,
    unique_authors INTEGER NOT NULL,
    latest_post_at INTEGER NOT NULL,
    score REAL NOT NULL,
    computed_at INTEGER NOT NULL
);

CREATE TABLE sync_state (
    pubkey TEXT PRIMARY KEY,
    last_synced_at INTEGER NOT NULL,
    last_post_timestamp INTEGER,
    last_interaction_timestamp INTEGER,
    FOREIGN KEY (pubkey) REFERENCES registrations(pubkey)
);

-- Cached device certificates for registered users (for verifying device-signed posts)
CREATE TABLE peer_device_certificates (
    identity_pubkey TEXT NOT NULL,
    device_pubkey TEXT NOT NULL,
    certificate_json TEXT NOT NULL,
    announcement_version INTEGER NOT NULL DEFAULT 0,
    cached_at INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, device_pubkey)
);

-- Cached device revocations (to reject posts from revoked devices)
CREATE TABLE device_revocations (
    identity_pubkey TEXT NOT NULL,
    revoked_device_pubkey TEXT NOT NULL,
    revoked_at INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, revoked_device_pubkey)
);

```

---

## HTTP API

All endpoints under `/api/v1/`. Server also returns basic HTML at `GET /`.

### Endpoints

#### Server Info

```
GET /api/v1/info

Response: {
    name, description, version, node_id,
    registered_users, total_posts,
    uptime_seconds, registration_open
}
```

#### Registration

```
POST /api/v1/register
Body: { pubkey, server_url, timestamp, action, signature }
Response (201): { pubkey, registered_at, message }
Errors: 400 (bad sig/timestamp), 409 (exists), 403 (closed)

DELETE /api/v1/register
Body: { pubkey, server_url, timestamp, action: "unregister", signature }
Server verifies via verify_registration_signature() from shared crate.
Response (200): { message }
```

#### User Directory

```
GET /api/v1/users?limit=20&offset=0
Response: { users: [...], total, limit, offset }

GET /api/v1/users/search?q=alice&limit=20
Response: { users: [...], total, query }

GET /api/v1/users/:pubkey
Response: { pubkey, display_name, bio, avatar_hash, registered_at, post_count, latest_post_at }
```

#### Posts

```
GET /api/v1/users/:pubkey/posts?limit=50&before=<timestamp>
Response: { posts: [...] }

GET /api/v1/posts/search?q=rust+iroh&limit=20&offset=0
Response: { posts: [...], total, query }
```

#### Interactions

```
GET /api/v1/users/:pubkey/interactions?limit=50&before=<timestamp>
Response: { interactions: [...] }

GET /api/v1/posts/:author/:post_id/interactions
Response: { interactions: [...], like_count: number }
```

#### Feed (Global)

```
GET /api/v1/feed?limit=50&before=<timestamp>
Response: { posts: [...] }

GET /api/v1/feed?limit=50&before=<timestamp>&authors=<pk1>,<pk2>
Optional author filter for custom feeds.
```

#### Trending

```
GET /api/v1/trending?limit=10
Response: { hashtags: [...], computed_at }

GET /api/v1/trending/posts?limit=20
Response: { posts: [...] }
```

### Middleware

- **Rate limiting** via `tower::limit`: registration 5/hr/IP, search 60/min/IP, reads 120/min/IP
- **CORS** enabled for all GET endpoints
- **Request logging** via `tower-http::trace`

---

## Trending Algorithm

### Hashtag extraction

Regex: `#[a-zA-Z0-9_]+`, normalized to lowercase.

### Scoring formula (per hashtag, over 24-hour window)

```
score = (post_count * author_weight * recency_factor) / age_decay

post_count    = posts containing the hashtag in the window
author_weight = sqrt(unique_authors)
recency_factor = sum(1.0 / (1.0 + hours_since_post)) for each post
age_decay     = 1.0 + (hours_since_oldest_post / 24.0)
```

- `sqrt(unique_authors)` prevents one user spamming a tag from dominating
- `recency_factor` weights newer posts higher
- `age_decay` reduces stale bursts

### Trending post score

```
post_score = (1 + min(hashtag_boost, 3)) * (1.0 / (1.0 + hours_since_post)^1.5)
```

Where `hashtag_boost` is the number of currently-trending hashtags in the post.

### Computation

Background task recomputes every 5 minutes. Results stored in `trending_hashtags` table. API reads always serve precomputed data.

---

## Client Integration

### New Tauri commands

```
add_server(url)              -- Fetch /api/v1/info, store connection
remove_server(url)           -- Remove stored connection
list_servers()               -- List all stored servers with status
register_with_server(url)    -- Sign + POST /api/v1/register
unregister_from_server(url)  -- Sign + DELETE /api/v1/register
server_search_posts(url, q)  -- Query /api/v1/posts/search
server_search_users(url, q)  -- Query /api/v1/users/search
server_get_feed(url, ...)    -- Query /api/v1/feed
server_get_trending(url)     -- Query /api/v1/trending
server_discover_users(url)   -- Query /api/v1/users
```

### New dependency

Add `reqwest` to `src-tauri/Cargo.toml` for HTTP client.

### New storage

Add a `servers` table to the client's SQLite database:

```sql
CREATE TABLE IF NOT EXISTS servers (
    url TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    is_registered INTEGER NOT NULL DEFAULT 0,
    added_at INTEGER NOT NULL
);
```

### New frontend pages

**`/servers` page:**

- List connected servers with status (online/offline)
- Add server by URL
- Register/unregister with each server

**`/discover` page (or integrated into servers):**

- Browse user directory from a selected server
- Search users by name
- "Follow" button for discovered users

**Search integration in feed:**

- When servers are configured, show search bar
- Results from server's FTS endpoint

**Trending section:**

- Display trending hashtags from connected server
- Click hashtag to search

### New TypeScript types

```typescript
interface ServerInfo {
  name: string;
  description: string;
  version: string;
  node_id: string;
  registered_users: number;
  total_posts: number;
  registration_open: boolean;
}
interface StoredServer {
  url: string;
  name: string;
  is_registered: boolean;
  added_at: number;
  status: "online" | "offline" | "unknown";
}
interface ServerUser {
  pubkey: string;
  display_name: string | null;
  bio: string | null;
  post_count: number;
}
interface TrendingHashtag {
  tag: string;
  post_count: number;
  unique_authors: number;
  score: number;
}
```

---

## Server Configuration

TOML config file:

```toml
[server]
name = "My Iroh Social Server"
description = "A community aggregation server"
listen_addr = "0.0.0.0:3000"
data_dir = "/var/lib/iroh-social-server"
public_url = "https://social.example.com"
registration_open = true

[limits]
max_registered_users = 1000
max_posts_per_user = 10000
rate_limit_requests_per_minute = 120

[sync]
interval_minutes = 15
startup_sync = true
max_concurrent_syncs = 10

[trending]
recompute_interval_minutes = 5
window_hours = 24
```

CLI (via `clap`):

```
iroh-social-server [OPTIONS]
  -c, --config <PATH>    Config file path (default: ./config.toml)
  --data-dir <PATH>      Override data directory
  --port <PORT>          Override listen port
```

---

## Federation (Phase 2)

Planned but not in initial scope. Servers would peer over iroh QUIC with a custom ALPN:

```
ALPN: b"iroh-social/federation/1"
```

What gets shared between servers:

- Registered user lists (pubkeys + profiles)
- Post metadata (other servers fetch full posts from users via P2P)
- Trending data

What does NOT get shared:

- Media blobs (fetch from users directly)
- User credentials

Federation uses iroh's QUIC transport (not HTTP) for NAT traversal and consistent P2P architecture.

---

## Implementation Roadmap

### Phase 1: Workspace Refactor (DONE)

- [x] Create workspace root `Cargo.toml`
- [x] Create `crates/iroh-social-types/` with types extracted from `src-tauri/`
- [x] Update `src-tauri/Cargo.toml` to use workspace deps and depend on shared crate
- [x] Verify Tauri app builds and runs unchanged

### Phase 2: Server Core

- [ ] Create `crates/iroh-social-server/` skeleton with `main.rs` and config
- [ ] Implement sqlx storage layer with migrations (SQLite + FTS5)
- [ ] Implement Iroh node setup (endpoint, gossip, sync handler -- no Tauri)
- [ ] Implement registration verification (ed25519 signature check)
- [ ] Implement device certificate caching and two-step verification for posts, interactions, and profiles (depends on linked-devices types)
- [ ] Implement `get_certificate` callback against sqlx storage for use with shared verification functions
- [ ] Handle `SyncFrame::DeviceAnnouncements` variant during sync to obtain missed device certificates (announcements are sent before posts/interactions per the frame ordering protocol)
- [ ] Verify profile signatures received via sync (`SyncSummary.profile`) using `verify_profile_signature()` with `SyncRequest.author` as expected identity
- [ ] Add `registration.rs` module to shared crate with `RegistrationPayload`, `RegistrationRequest`, `sign_registration()`, and `verify_registration_signature()` functions

### Phase 3: Server API + Ingestion

- [ ] Set up axum with middleware (CORS, rate limiting, logging)
- [ ] Implement endpoints: `/info`, `/register`, `/users`, `/feed`, `/posts/search`, `/trending`
- [ ] Implement interaction endpoints: `/users/:pubkey/interactions`, `/posts/:author/:id/interactions`
- [ ] Implement ingestion manager (gossip subscriber + sync scheduler)
- [ ] Handle all gossip variants: `NewPost`, `DeletePost` (verify via `verify_delete_post_signature()`), `ProfileUpdate` (verify via `verify_profile_signature()`), `NewInteraction`, `DeleteInteraction` (verify via `verify_delete_interaction_signature()`), `LinkedDevices`, `DeviceRevocation`
- [ ] Validate each gossip variant against the topic owner (per-variant author/identity matching)
- [ ] Implement retroactive cleanup on `DeviceRevocation`: delete posts/interactions from revoked device with timestamps after revocation
- [ ] Implement trending computation background task

### Phase 4: Client Integration

- [ ] Add `reqwest` to Tauri app
- [ ] Add servers table to client SQLite storage
- [ ] Implement Tauri commands for server interaction
- [ ] Build `/servers` page in Svelte
- [ ] Integrate search and discover into UI

### Phase 5: Polish

- [ ] Error handling and logging
- [ ] Server health check / metrics endpoint
- [ ] Sync fallback to secondary devices: when sync to a user's primary device fails, attempt sync from secondary devices discovered via cached `LinkedDevicesAnnouncement`
- [ ] Stub federation module with reserved ALPN
- [ ] Documentation and deployment guide
