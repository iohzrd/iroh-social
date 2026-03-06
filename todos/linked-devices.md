# Linked Devices - Design Document

Link multiple devices (phone, desktop, tablet) to a single identity. Each device has its own unique Ed25519 keypair. The primary device's public key remains the user's identity. Secondary devices are authorized via signed certificates and discovered via gossip announcements.

This design is inspired by Keybase's per-device key model, Matrix's cross-signing, and Signal's per-device session architecture -- adapted for a serverless P2P network.

## Table of Contents

- [Design Principles](#design-principles)
- [Prior Art](#prior-art)
- [Identity Model](#identity-model)
- [Device Certificate](#device-certificate)
- [Device Discovery](#device-discovery)
- [Pairing Protocol](#pairing-protocol)
- [Signing and Verification](#signing-and-verification)
- [DM Multi-Device](#dm-multi-device)
- [Device Sync](#device-sync)
- [Device Revocation](#device-revocation)
- [Storage](#storage)
- [Client Integration](#client-integration)
- [Community Server Considerations](#community-server-considerations)
- [Implementation Roadmap](#implementation-roadmap)

---

## Design Principles

1. **Per-device keys, never shared** -- Each device generates its own Ed25519 keypair locally. The identity secret key never leaves the primary device. Compromising a secondary device does not compromise the user's identity.
2. **Certificate-based delegation** -- The primary device signs a certificate authorizing each secondary device's public key. This is a two-step verification chain: identity key signed certificate, device key signed content.
3. **No server required** -- Pairing, discovery, and sync happen directly between devices over QUIC (iroh transport). No relay or cloud service stores device keys or certificates.
4. **Independent DM sessions** -- Each device maintains its own Double Ratchet sessions with peers. No ratchet synchronization needed. This eliminates the hardest problem in multi-device encrypted messaging.
5. **Clean revocation** -- Revoking a device is a signed gossip message. The user's identity is unaffected. No key rotation, no identity change, no disruption to existing conversations on other devices.
6. **Primary authority** -- Only the primary device can authorize new devices, revoke devices, and update the device list. Secondary devices can post, interact, message, and manage follows.

---

## Prior Art

This design draws from production multi-device systems:

### Signal (Sesame Algorithm)

Signal shares the identity key across all devices but gives each device its own prekeys and independent Double Ratchet sessions. The Sesame algorithm manages per-device-pair sessions. Message fanout is client-side: the sender encrypts separately for each of the recipient's devices, and also for their own other devices. We adopt Signal's per-device session model and client-side fanout for DMs.

### Matrix (Cross-Signing)

Matrix uses a three-level key hierarchy: master key (root of trust) signs a self-signing key, which signs individual device keys. Each device has its own Curve25519 + Ed25519 keypair. Megolm sessions handle group encryption with per-device key distribution via Olm. We adopt a simplified two-level version: identity key directly signs device certificates.

### Keybase (Sigchain + Per-User Key)

Keybase gave each device its own NaCl keypair (never shared). An append-only sigchain recorded device additions and revocations. A Per-User Key (PUK) provided account-level encryption, rotated on device revocation. The KEX protocol handled device pairing via word-based shared secrets. We adopt Keybase's per-device key philosophy and versioned device announcements (simpler than a full sigchain).

### Secure Scuttlebutt (Fusion Identity)

In SSB, each device is a separate feed with its own Ed25519 key. The Fusion Identity spec attempted to link feeds by having members exchange a shared fusion identity key. We avoid this complexity: our identity remains the primary's pubkey, and devices prove authority via certificates rather than sharing keys.

### Nostr (NIP-26 Delegation)

NIP-26 defined delegated event signing where a master key signs a token authorizing a sub-key. It was marked "unrecommended" by the Nostr community due to fragile string-based conditions and no revocation mechanism. We use binary certificates with explicit capabilities and gossip-based revocation instead.

---

## Identity Model

### Current State

Each device generates its own Ed25519 keypair (`identity.key`). The public key is the user's identity (iroh `EndpointId` / pubkey string). There is no concept of multiple devices sharing an identity.

The same Ed25519 key serves as:
- User identity (the pubkey string used everywhere)
- Post/interaction signing key
- DM session key (converted to X25519 for Noise IK handshake)
- iroh endpoint identity (NodeId)
- Gossip topic seed (`user_feed_topic(pubkey)`)

### New Model

```
User Identity = Primary Device's Ed25519 Public Key (unchanged)

Primary Device (e.g., Desktop)
  - Holds the identity keypair (identity.key, as today)
  - Signs DeviceCertificates for secondary devices
  - Can revoke any device
  - Posts signed with identity key directly (no certificate needed)

Secondary Device (e.g., Phone)
  - Generates its own Ed25519 keypair locally (identity.key on this device)
  - Has a different iroh NodeId than the primary
  - Holds a DeviceCertificate signed by the primary
  - Signs posts/interactions with its OWN device key
  - Establishes its own DM sessions with its OWN key
  - The identity secret key NEVER touches this device
```

The key insight: the `author` field on posts remains the primary's pubkey (the user's identity). A new `device_pubkey` field identifies which device actually signed the content. Peers verify a two-step chain: (1) certificate proves the identity authorized this device, (2) signature proves this device produced this content.

### Device Info via Announcement Only

The `Profile` struct does NOT gain a devices field. Device information comes exclusively from `LinkedDevicesAnnouncement`, which peers cache locally. This avoids duplicating device state between two structures that must stay in sync. Peers who need to display, verify, or DM target devices use the cached announcement.

---

## Device Certificate

### Format

Inspired by Tor's lightweight binary certificate format. Fixed-size fields, no parsing ambiguity, self-contained for verification.

```rust
/// A certificate authorizing a device to act on behalf of an identity.
/// Signed by the identity key (primary device's Ed25519 secret key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCertificate {
    /// Format version (currently 1).
    pub version: u8,
    /// The user's identity public key (primary device's pubkey).
    pub identity_pubkey: String,
    /// The authorized device's public key (its own EndpointId).
    pub device_pubkey: String,
    /// Human-readable device name (e.g. "Phone", "Laptop").
    pub device_name: String,
    /// When this certificate was issued (Unix timestamp ms).
    pub issued_at: u64,
    /// When this certificate expires (Unix timestamp ms, 0 = no expiry).
    pub expires_at: u64,
    /// Capability bitmask controlling what the device can do.
    pub capabilities: u32,
    /// Ed25519 signature from the identity key over the canonical signing bytes.
    pub signature: String,
}
```

### Capabilities Bitmask

```rust
pub const CAP_ALL: u32       = 0xFFFF;  // Full authority
```

The `capabilities` field is reserved for future use (e.g., restricting a device to post-only). In v1, all certificates are issued with `CAP_ALL` and verification does not check capabilities. The field is included in the signing bytes so it can be enforced in a future version without changing the certificate format.

### Canonical Signing Bytes

The certificate signature covers a deterministic JSON representation of all fields except the signature itself (same pattern as post signing in `signing.rs`):

```rust
fn certificate_signing_bytes(cert: &DeviceCertificate) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": cert.version,
        "identity_pubkey": cert.identity_pubkey,
        "device_pubkey": cert.device_pubkey,
        "device_name": cert.device_name,
        "issued_at": cert.issued_at,
        "expires_at": cert.expires_at,
        "capabilities": cert.capabilities,
    }))
    .expect("json serialization should not fail")
}
```

### Certificate Lifecycle

1. **Creation**: Primary device signs a certificate when a secondary device is paired.
2. **Distribution**: Certificate is sent to the secondary device during pairing. Also included in `LinkedDevicesAnnouncement` for peer discovery.
3. **Verification**: Any peer can verify a certificate by checking the signature against the `identity_pubkey`.
4. **Expiry**: If `expires_at > 0` and the current time exceeds it, the certificate is invalid. The secondary must re-pair or receive a renewed certificate.
5. **Revocation**: See [Device Revocation](#device-revocation).

### Primary Device Certificate

The primary device does NOT need a certificate. When `device_pubkey == author` (identity pubkey), verification falls back to the current direct signature check. This maintains backward compatibility and avoids a self-referential certificate.

---

## Device Discovery

Peers need to know which devices belong to an identity so they can:
- Verify posts from secondary devices
- Send DMs to all of a user's devices
- Display device information in the UI

### LinkedDevicesAnnouncement

Published via gossip on the user's feed topic (`user_feed_topic(identity_pubkey)`). This is the same topic used for posts and profile updates.

```rust
/// Announces the set of devices linked to an identity.
/// Published via gossip. Peers cache the latest version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedDevicesAnnouncement {
    /// The user's identity public key.
    pub identity_pubkey: String,
    /// All currently authorized devices (including primary).
    pub devices: Vec<DeviceAnnouncement>,
    /// Monotonically increasing version number. Latest wins.
    /// Inspired by Keybase's sigchain sequence numbers.
    pub version: u64,
    /// When this announcement was created (Unix timestamp ms).
    pub timestamp: u64,
    /// Ed25519 signature from the identity key over the canonical bytes.
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAnnouncement {
    /// The device's own public key (its iroh NodeId).
    pub device_pubkey: String,
    /// Human-readable name.
    pub device_name: String,
    /// Whether this is the primary device.
    pub is_primary: bool,
    /// The full DeviceCertificate (for secondary devices).
    /// None for the primary device.
    pub certificate: Option<DeviceCertificate>,
}
```

### GossipMessage Extension

Update the existing `GossipMessage` enum. The `LinkedDevices` and `DeviceRevocation` variants are new. The `DeletePost` and `DeleteInteraction` variants gain `device_pubkey` and `signature` fields so that delete messages can be verified -- with multiple NodeIds publishing to the same gossip topic, unsigned deletes would allow any topic participant to forge deletions.

```rust
pub enum GossipMessage {
    NewPost(Post),
    DeletePost {
        id: String,
        author: String,
        device_pubkey: String,  // CHANGED: the device that signed this delete
        signature: String,      // CHANGED: signed by device_pubkey's secret key
    },
    ProfileUpdate(Profile),
    NewInteraction(Interaction),
    DeleteInteraction {
        id: String,
        author: String,
        device_pubkey: String,  // CHANGED: the device that signed this delete
        signature: String,      // CHANGED: signed by device_pubkey's secret key
    },
    // New:
    LinkedDevices(LinkedDevicesAnnouncement),
    DeviceRevocation(DeviceRevocation),
}
```

#### Delete Signing

```rust
fn delete_post_signing_bytes(id: &str, author: &str, device_pubkey: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "action": "delete_post",
        "id": id,
        "author": author,
        "device_pubkey": device_pubkey,
    }))
    .expect("json serialization should not fail")
}

fn delete_interaction_signing_bytes(id: &str, author: &str, device_pubkey: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "action": "delete_interaction",
        "id": id,
        "author": author,
        "device_pubkey": device_pubkey,
    }))
    .expect("json serialization should not fail")
}
```

#### Delete Sign/Verify API

```rust
pub fn sign_delete_post(
    id: &str, author: &str, device_pubkey: &str, secret_key: &SecretKey,
) -> String {
    let bytes = delete_post_signing_bytes(id, author, device_pubkey);
    let sig = secret_key.sign(&bytes);
    signature_to_hex(&sig)
}

pub fn verify_delete_post_signature(
    id: &str, author: &str, device_pubkey: &str, signature: &str,
    get_certificate: impl Fn(&str, &str) -> Option<DeviceCertificate>,
) -> Result<(), String> {
    let sig = hex_to_signature(signature)?;
    let effective_device = if device_pubkey.is_empty() { author } else { device_pubkey };
    let device_key: PublicKey = effective_device
        .parse()
        .map_err(|e| format!("invalid device pubkey: {e}"))?;
    let bytes = delete_post_signing_bytes(id, author, device_pubkey);
    device_key
        .verify(&bytes, &sig)
        .map_err(|_| "delete post signature verification failed".to_string())?;
    if effective_device != author {
        let cert = get_certificate(author, effective_device)
            .ok_or("no certificate found for device")?;
        verify_certificate(&cert)?;
    }
    Ok(())
}

pub fn sign_delete_interaction(
    id: &str, author: &str, device_pubkey: &str, secret_key: &SecretKey,
) -> String {
    let bytes = delete_interaction_signing_bytes(id, author, device_pubkey);
    let sig = secret_key.sign(&bytes);
    signature_to_hex(&sig)
}

pub fn verify_delete_interaction_signature(
    id: &str, author: &str, device_pubkey: &str, signature: &str,
    get_certificate: impl Fn(&str, &str) -> Option<DeviceCertificate>,
) -> Result<(), String> {
    let sig = hex_to_signature(signature)?;
    let effective_device = if device_pubkey.is_empty() { author } else { device_pubkey };
    let device_key: PublicKey = effective_device
        .parse()
        .map_err(|e| format!("invalid device pubkey: {e}"))?;
    let bytes = delete_interaction_signing_bytes(id, author, device_pubkey);
    device_key
        .verify(&bytes, &sig)
        .map_err(|_| "delete interaction signature verification failed".to_string())?;
    if effective_device != author {
        let cert = get_certificate(author, effective_device)
            .ok_or("no certificate found for device")?;
        verify_certificate(&cert)?;
    }
    Ok(())
}
```

Verification follows the same two-step chain as posts and interactions.

### Peer-Side Caching

When a peer receives a `LinkedDevicesAnnouncement`:

1. Verify the signature against `identity_pubkey`.
2. Check that `version` is greater than any previously cached version for this identity.
3. Verify each device's certificate (if present) against the identity key.
4. Store the announcement (replace previous version).
5. Use the device list when sending DMs and verifying posts.

Announcements are also sent via the existing sync protocol so that peers who come online later receive them. This requires extending `SyncFrame` with a new variant:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub enum SyncFrame {
    Posts(Vec<Post>),
    Interactions(Vec<Interaction>),
    DeviceAnnouncements(Vec<LinkedDevicesAnnouncement>),  // NEW
}
```

**Frame ordering**: During a sync session, `DeviceAnnouncements` frames are sent **before** `Posts` and `Interactions` frames. This ordering is critical: the recipient must cache device certificates before it can perform two-step verification on device-signed posts and interactions. Both sides exchange their cached `LinkedDevicesAnnouncement` for the author being synced (if any). This ensures a peer or server that missed the gossip announcement still receives device certificates and can verify secondary-device posts. The SYNC_ALPN bumps to version 4 to reflect this wire format change.

---

## Pairing Protocol

### Overview

Pairing is simpler than the original shared-key design because the identity secret key never leaves the primary device. Only a certificate and social graph data are transferred.

```
Primary                              Secondary
   |                                     |
   |  1. User taps "Link New Device"     |
   |     Primary generates:              |
   |     - one-time secret (32 bytes)    |
   |     - QR code / pairing code        |
   |                                     |
   |  2. User scans QR / enters code     |
   |     on secondary device             |
   |                         <---------  |
   |                                     |
   |  3. Secondary connects via QUIC     |
   |     on LINK_ALPN                    |
   |     <----------------------------   |
   |                                     |
   |  4. Noise IK + PSK handshake        |
   |     (QR secret as pre-shared key)   |
   |     ---------------------------->   |
   |     <----------------------------   |
   |                                     |
   |  5. Secondary sends its pubkey      |
   |     and device name                 |
   |     <----------------------------   |
   |                                     |
   |  6. Primary creates and signs       |
   |     DeviceCertificate for           |
   |     secondary's pubkey              |
   |                                     |
   |  7. Primary sends LinkBundle:       |
   |     - DeviceCertificate             |
   |     - Profile data                  |
   |     - Follow list                   |
   |     - Bookmarks, mutes, blocks      |
   |     - NO secret key                 |
   |     ---------------------------->   |
   |                                     |
   |  8. Secondary confirms receipt      |
   |     <----------------------------   |
   |                                     |
   |  9. Primary publishes updated       |
   |     LinkedDevicesAnnouncement       |
   |     via gossip                      |
   |                                     |
   |  [Paired. Secondary begins          |
   |   independent operation.]           |
```

### ALPN

```rust
pub const LINK_ALPN: &[u8] = b"iroh-social/link/1";
```

### QR Code Content

```rust
/// Encoded in the QR code displayed by the primary device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkQrPayload {
    /// Primary device's EndpointId (identity pubkey).
    pub identity_pubkey: String,
    /// One-time secret for Noise PSK (32 bytes, base64-encoded).
    pub secret: String,
    /// Primary device's network address for direct connection.
    pub addrs: String,
}
```

Serialized as a compact URI: `iroh-social://link?pk=<pubkey>&s=<secret>&a=<addrs>`

The QR code expires after 60 seconds. The one-time secret is generated fresh each time the user initiates pairing.

### Noise IK + PSK Handshake

Use `Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s` -- the same Noise IK pattern used for DM sessions but with an additional pre-shared key (the QR secret). This ensures:

1. **Authentication** -- Both sides know each other's static keys after handshake.
2. **QR verification** -- Only someone who scanned the QR code knows the PSK, preventing MITM.
3. **Forward secrecy** -- Ephemeral keys ensure the pairing channel cannot be decrypted later.

Note: The secondary device's Noise static key is its own X25519 key (derived from its own Ed25519 key), NOT the primary's. This is a key difference from the old design where both sides would use the same identity key.

### LinkBundle

Data sent from primary to secondary during pairing. Does NOT include the identity secret key.

```rust
/// Data bundle sent from primary to secondary during device pairing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkBundle {
    /// Signed certificate authorizing this device.
    pub certificate: DeviceCertificate,
    /// The user's identity pubkey (redundant with certificate, for convenience).
    pub identity_pubkey: String,
    /// User profile.
    pub profile: Profile,
    /// Follow list.
    pub follows: Vec<FollowEntry>,
    /// Bookmarked post IDs.
    pub bookmarks: Vec<(String, String)>,  // (author, post_id)
    /// Blocked user pubkeys.
    pub blocked_users: Vec<String>,
    /// Muted user pubkeys.
    pub muted_users: Vec<String>,
}
```

The `profile` field must be signed before inclusion in the bundle. The primary device calls `sign_profile(&mut profile, &identity_secret_key)` so the secondary device receives a verifiable profile it can store and serve.

Notable omissions compared to old design:
- **No `master_secret_key`** -- the identity key never leaves the primary.
- **No `ratchet_sessions`** -- each device will establish its own DM sessions independently.
- **No `conversations`** -- DM history arrives naturally as peers send to the new device.

### Desktop-to-Desktop Pairing

When a camera is unavailable (e.g., pairing two desktops), the QR payload can be displayed as a text code that the user copies and pastes into the secondary device's pairing dialog.

---

## Signing and Verification

**Shared utility functions**: The signing utility functions `signature_to_hex()` and `hex_to_signature()` in `signing.rs` must be made `pub` so they can be used by `devices.rs` (certificate signing), `registration.rs` (registration signing), and any other module that needs Ed25519 signature hex encoding.

### Post Signing (Changed)

Posts gain a `device_pubkey` field. The signing key depends on whether this is the primary or a secondary device.

```rust
pub struct Post {
    pub id: String,
    pub author: String,           // identity pubkey (unchanged)
    pub content: String,
    pub timestamp: u64,
    pub media: Vec<MediaAttachment>,
    pub reply_to: Option<String>,
    pub reply_to_author: Option<String>,
    pub quote_of: Option<String>,
    pub quote_of_author: Option<String>,
    pub device_pubkey: String,    // NEW: the device that signed this post
    pub signature: String,        // signed by device_pubkey's secret key
}
```

#### Signing

```rust
/// Sign a post. Uses the device's own secret key.
/// The `author` field is always the identity pubkey.
/// The `device_pubkey` field is this device's pubkey.
pub fn sign_post(post: &mut Post, device_secret_key: &SecretKey) {
    let bytes = post_signing_bytes(post);
    let sig = device_secret_key.sign(&bytes);
    post.signature = signature_to_hex(&sig);
}
```

The canonical signing bytes include `device_pubkey`:

```rust
fn post_signing_bytes(post: &Post) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": post.id,
        "author": post.author,
        "content": post.content,
        "timestamp": post.timestamp,
        "media": post.media,
        "reply_to": post.reply_to,
        "reply_to_author": post.reply_to_author,
        "quote_of": post.quote_of,
        "quote_of_author": post.quote_of_author,
        "device_pubkey": post.device_pubkey,
    }))
    .expect("json serialization should not fail")
}
```

#### Verification (Two-Step Chain)

```rust
/// Verify a post's signature. Two cases:
/// 1. Primary device: device_pubkey is empty or == author -> direct verification.
/// 2. Secondary device: device_pubkey != author -> verify certificate chain.
pub fn verify_post_signature(
    post: &Post,
    get_certificate: impl Fn(&str, &str) -> Option<DeviceCertificate>,
) -> Result<(), String> {
    let sig = hex_to_signature(&post.signature)?;

    // Treat empty device_pubkey as the author (primary device / pre-migration post)
    let effective_device = if post.device_pubkey.is_empty() {
        &post.author
    } else {
        &post.device_pubkey
    };

    let device_pubkey: PublicKey = effective_device
        .parse()
        .map_err(|e| format!("invalid device pubkey: {e}"))?;
    let bytes = post_signing_bytes(post);

    // Step 1: Verify the content signature against the device key
    device_pubkey
        .verify(&bytes, &sig)
        .map_err(|_| "device signature verification failed".to_string())?;

    // Step 2: If device != author, verify the device is authorized
    if effective_device != post.author {
        let cert = get_certificate(&post.author, effective_device)
            .ok_or("no certificate found for device")?;
        verify_certificate(&cert)?;
    }

    Ok(())
}
```

#### Certificate Verification

```rust
/// Verify a DeviceCertificate is valid (signature + expiry).
/// Public: used by both client (gossip.rs, sync.rs) and server (ingestion.rs).
pub fn verify_certificate(cert: &DeviceCertificate) -> Result<(), String> {
    // 1. Verify the certificate signature against the identity key
    let identity_key: PublicKey = cert.identity_pubkey
        .parse()
        .map_err(|e| format!("invalid identity pubkey in cert: {e}"))?;
    let sig = hex_to_signature(&cert.signature)?;
    let bytes = certificate_signing_bytes(cert);
    identity_key
        .verify(&bytes, &sig)
        .map_err(|_| "certificate signature verification failed".to_string())?;

    // 2. Check expiry (0 = no expiry)
    if cert.expires_at > 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        if now > cert.expires_at {
            return Err("certificate expired".to_string());
        }
    }

    Ok(())
}
```

### Interaction Signing (Same Pattern)

Interactions gain the same `device_pubkey` field and two-step verification:

```rust
pub struct Interaction {
    pub id: String,
    pub author: String,           // identity pubkey (unchanged)
    pub kind: InteractionKind,
    pub target_post_id: String,
    pub target_author: String,
    pub timestamp: u64,
    pub device_pubkey: String,    // NEW: the device that signed this interaction
    pub signature: String,        // signed by device_pubkey's secret key
}
```

#### Signing

```rust
pub fn sign_interaction(interaction: &mut Interaction, device_secret_key: &SecretKey) {
    let bytes = interaction_signing_bytes(interaction);
    let sig = device_secret_key.sign(&bytes);
    interaction.signature = signature_to_hex(&sig);
}
```

The canonical signing bytes include `device_pubkey`:

```rust
fn interaction_signing_bytes(interaction: &Interaction) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": interaction.id,
        "author": interaction.author,
        "kind": interaction.kind,
        "target_post_id": interaction.target_post_id,
        "target_author": interaction.target_author,
        "timestamp": interaction.timestamp,
        "device_pubkey": interaction.device_pubkey,
    }))
    .expect("json serialization should not fail")
}
```

#### Verification (Two-Step Chain)

```rust
pub fn verify_interaction_signature(
    interaction: &Interaction,
    get_certificate: impl Fn(&str, &str) -> Option<DeviceCertificate>,
) -> Result<(), String> {
    let sig = hex_to_signature(&interaction.signature)?;

    // Treat empty device_pubkey as the author (primary device / pre-migration post)
    let effective_device = if interaction.device_pubkey.is_empty() {
        &interaction.author
    } else {
        &interaction.device_pubkey
    };

    let device_pubkey: PublicKey = effective_device
        .parse()
        .map_err(|e| format!("invalid device pubkey: {e}"))?;
    let bytes = interaction_signing_bytes(interaction);

    // Step 1: Verify the content signature against the device key
    device_pubkey
        .verify(&bytes, &sig)
        .map_err(|_| "device signature verification failed".to_string())?;

    // Step 2: If device != author, verify the device is authorized
    if effective_device != interaction.author {
        let cert = get_certificate(&interaction.author, effective_device)
            .ok_or("no certificate found for device")?;
        verify_certificate(&cert)?;
    }

    Ok(())
}
```

### Profile Signing (New)

Profiles currently have no signature. With linked devices, multiple NodeIds publish to the same gossip topic, so a malicious node could inject a fake `ProfileUpdate`. For consistency with posts and interactions, profiles gain signing:

```rust
pub struct Profile {
    pub display_name: String,
    pub bio: String,
    pub avatar_hash: Option<String>,
    pub avatar_ticket: Option<String>,
    pub is_private: bool,
    pub device_pubkey: String,    // NEW: the device that signed this profile update
    pub signature: String,        // NEW: signed by device_pubkey's secret key
}
```

#### Signing

```rust
fn profile_signing_bytes(profile: &Profile) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "display_name": profile.display_name,
        "bio": profile.bio,
        "avatar_hash": profile.avatar_hash,
        "avatar_ticket": profile.avatar_ticket,
        "is_private": profile.is_private,
        "device_pubkey": profile.device_pubkey,
    }))
    .expect("json serialization should not fail")
}

pub fn sign_profile(profile: &mut Profile, device_secret_key: &SecretKey) {
    let bytes = profile_signing_bytes(profile);
    let sig = device_secret_key.sign(&bytes);
    profile.signature = signature_to_hex(&sig);
}
```

#### Verification

```rust
pub fn verify_profile_signature(
    profile: &Profile,
    expected_identity: &str,
    get_certificate: impl Fn(&str, &str) -> Option<DeviceCertificate>,
) -> Result<(), String> {
    let sig = hex_to_signature(&profile.signature)?;

    // Treat empty device_pubkey as the expected identity (primary device)
    let effective_device = if profile.device_pubkey.is_empty() {
        expected_identity
    } else {
        &profile.device_pubkey
    };

    let device_pubkey: PublicKey = effective_device
        .parse()
        .map_err(|e| format!("invalid device pubkey: {e}"))?;
    let bytes = profile_signing_bytes(profile);

    device_pubkey
        .verify(&bytes, &sig)
        .map_err(|_| "profile signature verification failed".to_string())?;

    if effective_device != expected_identity {
        let cert = get_certificate(expected_identity, effective_device)
            .ok_or("no certificate found for device")?;
        verify_certificate(&cert)?;
    }

    Ok(())
}
```

Note: `verify_profile_signature` takes an `expected_identity` parameter because `Profile` does not contain an author field -- the identity is known from the gossip topic the update was received on.

**Profile verification applies to both gossip and sync paths.** Gossip-received profile updates use the topic owner as `expected_identity`. Sync-received profiles (in `SyncSummary.profile`) use `SyncRequest.author` as `expected_identity`. Both must be verified before storing.

### Certificate Caching Strategy

Certificates are obtained from:
1. **LinkedDevicesAnnouncement** (cached per identity, received via gossip)
2. **Direct request** via the device sync protocol (fallback if announcement is missing)

Peers cache certificates in the local `peer_device_certificates` table. On verification, the peer looks up the certificate by `(identity_pubkey, device_pubkey)`. If not found, verification fails and the peer can request the announcement from the gossip topic.

---

## DM Multi-Device

This is where per-device keys provide the biggest advantage. The ratchet synchronization problem -- the single hardest challenge in the original shared-key design -- is eliminated entirely.

### Architecture

Each device has its own X25519 key (derived from its own Ed25519 key). Each device independently establishes Noise IK handshakes and Double Ratchet sessions with peers. No two devices share ratchet state.

```
Alice (identity: IK_A)
  Device A1 (key: DK_A1, X25519: XK_A1)
  Device A2 (key: DK_A2, X25519: XK_A2)

Bob (identity: IK_B)
  Device B1 (key: DK_B1, X25519: XK_B1)

Ratchet sessions (all independent):
  (A1, B1): Noise IK(XK_A1, XK_B1) -> ratchet_A1_B1
  (A2, B1): Noise IK(XK_A2, XK_B1) -> ratchet_A2_B1
```

### DM Sending (Multi-Target)

When Bob sends a DM to Alice:

1. Bob looks up Alice's `LinkedDevicesAnnouncement` to find her device list: `[A1, A2]`.
2. For each device, Bob checks if he has an active ratchet session.
3. If no session exists, Bob initiates a Noise IK handshake with that device's X25519 key (derived from the device's Ed25519 pubkey) on `DM_ALPN`.
4. Bob encrypts the message separately for each device using that device's ratchet session.
5. Bob sends each encrypted copy to the corresponding device's iroh NodeId.

```rust
/// Send a DM to all of a peer's devices.
async fn send_dm_multi_device(
    &self,
    peer_identity: &str,
    content: &str,
) -> anyhow::Result<()> {
    let devices = self.get_peer_devices(peer_identity)?;
    for device in &devices {
        let ratchet = self.get_or_establish_session_with_device(
            &device.device_pubkey
        ).await?;
        let (header, ciphertext) = ratchet.encrypt(content.as_bytes());
        self.send_encrypted_to_device(&device.device_pubkey, header, ciphertext).await?;
    }
    Ok(())
}
```

### DM Receiving

The DM handler (`DmHandler`) remains largely unchanged. Each device independently:

1. Accepts incoming QUIC connections on `DM_ALPN`.
2. Performs Noise IK handshake using its own X25519 key.
3. Maintains its own ratchet session with the sending device.
4. Decrypts and stores messages locally.

The key change: during session establishment, both sides must present their identity so the other side can verify the device belongs to the expected identity. In the Noise IK pattern, the initiator already knows the responder's static key (the "K"), and Noise authenticates it during the handshake. But the responder learns the initiator's static key only during the handshake and has no way to map it to an identity without additional information. Both sides include identity info for symmetry and to handle bootstrap cases (e.g., the initiator hasn't cached the responder's announcement yet).

The `DmHandshake` wire format changes (new fields on both variants), so the ALPN bumps to version 2:

```rust
pub const DM_ALPN: &[u8] = b"iroh-social/dm/2";
```

```rust
pub enum DmHandshake {
    Init {
        noise_message: Vec<u8>,
        /// The initiator's identity pubkey (so the responder can verify
        /// this device is authorized to act for this identity).
        identity_pubkey: String,
        /// Certificate proving this device belongs to the claimed identity.
        /// None if this is the primary device (device_pubkey == identity_pubkey).
        device_certificate: Option<DeviceCertificate>,
    },
    Response {
        noise_message: Vec<u8>,
        /// The responder's identity pubkey.
        identity_pubkey: String,
        /// Certificate proving this device belongs to the claimed identity.
        /// None if this is the primary device (device_pubkey == identity_pubkey).
        device_certificate: Option<DeviceCertificate>,
    },
}
```

After the handshake completes, both sides verify:
1. The `device_pubkey` in the certificate (if present) matches the Noise-authenticated static key.
2. The certificate signature is valid against the claimed `identity_pubkey`.
3. If no certificate (primary device), the Noise static key must match `identity_pubkey` directly.
4. Optionally cross-check against a cached `LinkedDevicesAnnouncement` if available.

### EncryptedEnvelope Sender Field

The `EncryptedEnvelope.sender` field remains the **identity pubkey** (not the device pubkey). DM conversations are between identities, not individual devices. The sending device's identity is already established during the Noise IK handshake (via the `identity_pubkey` field in `DmHandshake`), so the `sender` field serves as a convenience for conversation routing and display.

### Own-Device Message Sync (Signal's Approach)

When you send a DM from device A1, your other device A2 does not automatically receive the message (since A2's ratchet with Bob is independent). DM history is synced between own devices via `DEVICE_SYNC_ALPN` during periodic device sync. Messages may not appear on the other device immediately, but they will arrive on the next sync cycle (60s intervals when both devices are online).

### Cost Analysis

The DM multi-target approach has costs:

| Factor | Single-device (current) | Multi-device (per-device keys) |
|--------|------------------------|-------------------------------|
| Ratchet sessions per peer | 1 | N (one per peer device) |
| Encryption operations per DM | 1 | N (one per recipient device) |
| Bandwidth per DM sent | 1x | Nx (one copy per device) |
| Ratchet state storage | 1 entry per peer | N entries per peer |
| Ratchet sync complexity | N/A | None (independent sessions) |

Where N = number of devices the recipient has. In practice N is 2-3 (phone + desktop, maybe tablet). DMs are small (typically < 1KB), so the bandwidth and computation overhead is negligible.

---

## Device Sync

Unlike the original design, device sync is much simpler because DM ratchet sessions are independent (no ratchet sync needed). Only "easy" data needs synchronization.

### What Needs Sync vs What Doesn't

| Data | Needs sync? | Why |
|------|-------------|-----|
| Profile | Yes | All devices should present the same profile |
| Follows | Yes | Social graph should be consistent |
| Posts | Yes | Gossip is best-effort; devices may miss each other's posts |
| Interactions | Yes | Likes from one device should appear on the other |
| DM ratchet sessions | No | Each device has independent sessions |
| DM message history | Yes (optional) | For conversation continuity on new devices |
| Bookmarks | Yes | User preference |
| Mutes / Blocks | Yes | Must be consistent for filtering |
| Notifications read state | Yes (optional) | For cross-device read tracking |

Note on posts: while both devices publish to the same gossip topic, gossip is best-effort. If Alice posts from her phone while her desktop is offline, the desktop may never receive that post via gossip (no intermediary had it, or the desktop wasn't subscribed yet). Without device sync, peers who later sync with the desktop would miss the phone's posts. Post sync between devices ensures each device has a complete history and can serve it to peers. The delta mechanism (posts after timestamp X) keeps sync efficient.

### Sync ALPN

```rust
pub const DEVICE_SYNC_ALPN: &[u8] = b"iroh-social/device-sync/1";
```

### Sync Protocol

A lightweight protocol between linked devices. Authentication: both sides verify each other's DeviceCertificate (or identity key for primary) during the QUIC connection.

```rust
/// Summary of what a device has, exchanged at sync start.
#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceSyncVector {
    /// This device's pubkey.
    pub device_pubkey: String,
    /// Timestamp of last profile update.
    pub profile_updated_at: u64,
    /// Timestamp of last follow list change.
    pub follows_updated_at: u64,
    /// Timestamp of last bookmark change.
    pub bookmarks_updated_at: u64,
    /// Timestamp of last mute/block change.
    pub moderation_updated_at: u64,
    /// Total post count and newest post timestamp (for post sync).
    pub post_count: u64,
    pub newest_post_timestamp: u64,
    /// Total interaction count and newest interaction timestamp.
    pub interaction_count: u64,
    pub newest_interaction_timestamp: u64,
    /// Per-conversation latest message timestamp (for DM history sync).
    pub dm_conversation_heads: Vec<(String, u64)>,
}
```

### Sync Flow

1. Device A connects to Device B on `DEVICE_SYNC_ALPN`.
2. Both exchange `DeviceSyncVector`.
3. Each device computes what the other is missing (timestamp comparison).
4. Missing data is streamed as length-prefixed JSON frames (same pattern as the existing sync protocol).
5. Both devices merge the received data.

### Conflict Resolution

| Category | Resolution |
|----------|-----------|
| Profile | Last-write-wins by timestamp |
| Follows | LWW-per-entry by timestamp |
| Bookmarks | Set union |
| Mutes/Blocks | LWW-per-entry by timestamp |
| Posts | Set union by post id (deduplicate) |
| Interactions | Set union by interaction id (deduplicate) |
| DM messages | Set union by message_id (deduplicate) |

**LWW-per-entry** (last-write-wins per individual entry): Each follow/mute/block record keeps a `state` (active or removed) and a `last_changed_at` timestamp. When syncing, both devices exchange their full list for the category. For each entry present on either side, the entry with the latest `last_changed_at` wins. This correctly propagates both adds and removes.

Why not set union for follows/mutes/blocks: "set union (add wins over remove)" means removals can never propagate. If Alice unfollows Bob on her phone while her desktop is offline, the next sync would re-add Bob from the desktop's list. The same problem applies to unmuting or unblocking -- the removal would silently revert. LWW-per-entry avoids this by letting the most recent action win regardless of direction.

Clock skew between devices of the same user is negligible (NTP keeps modern devices within ~1 second, and follow/mute/block actions are separated by minutes or hours).

Schema impact: the `follows`, `mutes`, and `blocks` tables need a `state` column and `last_changed_at` timestamp instead of deleting rows on unfollow/unmute/unblock. See [Storage](#storage) for migration details.

### Sync Triggers

- **On reconnection** -- When a linked device comes online, sync immediately.
- **Periodic** -- Every 60 seconds while both devices are online.

---

## Device Revocation

### Revocation Message

```rust
/// Signed by the identity key. Published via gossip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRevocation {
    /// The user's identity public key.
    pub identity_pubkey: String,
    /// The device being revoked.
    pub revoked_device_pubkey: String,
    /// When the revocation was issued (Unix timestamp ms).
    pub timestamp: u64,
    /// Ed25519 signature from the identity key.
    pub signature: String,
}
```

### Revocation Flow

1. User selects "Unlink Device" on the primary device.
2. Primary creates and signs a `DeviceRevocation`.
3. Primary publishes the revocation via gossip (`GossipMessage::DeviceRevocation`).
4. Primary publishes an updated `LinkedDevicesAnnouncement` (incremented version, without the revoked device).
5. Peers receive both messages and:
   - Remove the revoked device's certificate from their cache.
   - Cache the revocation in `device_revocations` table (for rejecting future content).
   - Reject any future posts or DMs signed by the revoked device key.
   - **Retroactive cleanup**: scan already-cached posts and interactions where `device_pubkey == revoked_device_pubkey` and `timestamp > revocation.timestamp`, and delete them. A revoked device may have published content between the revocation being issued and the peer receiving it.
   - Stop sending DMs to the revoked device.
   - Delete pending outbox entries targeting the revoked device (`WHERE peer_pubkey == revoked_device_pubkey`).

### Revocation on the Secondary

If the secondary device is online when revoked:

1. It receives the revocation via gossip (it's subscribed to its own identity's feed topic).
2. It displays a notification: "This device has been unlinked."
3. It can optionally wipe its data or continue operating as a standalone (new) identity.

If the secondary device is offline:

1. On next startup, it attempts to sync with the primary.
2. The primary refuses the sync and sends the revocation.
3. Or: the secondary discovers the revocation via gossip from peers.

### Security Considerations

- **Revocation is irreversible** -- once published via gossip, the revocation propagates to all peers. There is no "un-revoke."
- **Race condition** -- A revoked device could publish posts between the revocation being created and peers receiving it. Peers and servers must perform retroactive cleanup: on receiving a revocation, scan cached posts/interactions from the revoked device and delete those with timestamps after the revocation timestamp.
- **Primary compromise** -- If the primary device is lost or compromised, the user must create a new identity. This is inherent to the model where the primary key IS the identity. Future work could add recovery mechanisms (e.g., paper keys a la Keybase, or threshold recovery).

---

## Storage

### New Migration

```sql
-- Own device info (this device)
CREATE TABLE IF NOT EXISTS device_info (
    device_pubkey TEXT PRIMARY KEY,
    identity_pubkey TEXT NOT NULL,
    device_name TEXT NOT NULL,
    is_primary INTEGER NOT NULL DEFAULT 0,
    certificate_json TEXT,  -- NULL for primary device
    paired_at INTEGER NOT NULL
);

-- Known linked devices (own identity's other devices)
CREATE TABLE IF NOT EXISTS linked_devices (
    device_pubkey TEXT PRIMARY KEY,
    identity_pubkey TEXT NOT NULL,
    device_name TEXT NOT NULL,
    is_primary INTEGER NOT NULL DEFAULT 0,
    certificate_json TEXT,
    added_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL DEFAULT 0
);

-- Cached device certificates for OTHER users (peer device discovery)
CREATE TABLE IF NOT EXISTS peer_device_certificates (
    identity_pubkey TEXT NOT NULL,
    device_pubkey TEXT NOT NULL,
    certificate_json TEXT NOT NULL,
    announcement_version INTEGER NOT NULL DEFAULT 0,
    cached_at INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, device_pubkey)
);

-- Device sync state tracking
CREATE TABLE IF NOT EXISTS device_sync_state (
    device_pubkey TEXT PRIMARY KEY,
    sync_vector_json TEXT NOT NULL DEFAULT '{}',
    last_sync_at INTEGER NOT NULL DEFAULT 0
);

-- Cached device revocations (to reject posts from revoked devices)
CREATE TABLE IF NOT EXISTS device_revocations (
    identity_pubkey TEXT NOT NULL,
    revoked_device_pubkey TEXT NOT NULL,
    revoked_at INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, revoked_device_pubkey)
);
```

### Schema Changes to Existing Tables

Add `device_pubkey` column to posts and interactions:

```sql
ALTER TABLE posts ADD COLUMN device_pubkey TEXT NOT NULL DEFAULT '';
ALTER TABLE interactions ADD COLUMN device_pubkey TEXT NOT NULL DEFAULT '';
```

After adding the column, backfill and re-sign own posts and interactions. The new `post_signing_bytes()` and `interaction_signing_bytes()` include `device_pubkey` in the canonical JSON, so existing signatures (computed without `device_pubkey`) are invalid under the new format. Own content must be re-signed so it verifies correctly when served to peers via sync:

```rust
// Run during migration (Tauri app startup)
fn resign_own_content(storage: &Storage, my_pubkey: &str, secret_key: &SecretKey) -> Result<()> {
    // Re-sign own profile
    if let Ok(mut profile) = storage.get_profile(my_pubkey) {
        profile.device_pubkey = my_pubkey.to_string();
        sign_profile(&mut profile, secret_key);
        storage.update_profile_signature(my_pubkey, &profile.device_pubkey, &profile.signature)?;
    }

    // Re-sign own posts
    let own_posts = storage.get_posts_by_author(my_pubkey, usize::MAX, None, None)?;
    for mut post in own_posts {
        post.device_pubkey = my_pubkey.to_string();
        sign_post(&mut post, secret_key);
        storage.update_post_signature(&post.id, &post.author, &post.device_pubkey, &post.signature)?;
    }

    // Re-sign own interactions
    let own_interactions = storage.get_interactions_paged(my_pubkey, usize::MAX, 0)?;
    for mut interaction in own_interactions {
        interaction.device_pubkey = my_pubkey.to_string();
        sign_interaction(&mut interaction, secret_key);
        storage.update_interaction_signature(&interaction.id, &interaction.author, &interaction.device_pubkey, &interaction.signature)?;
    }

    Ok(())
}
```

For peers' cached posts/interactions: these are never served to others (only viewed locally), so stale signatures are harmless. The `device_pubkey` column is set to `author` for display purposes:

```sql
UPDATE posts SET device_pubkey = author WHERE device_pubkey = '';
UPDATE interactions SET device_pubkey = author WHERE device_pubkey = '';
```

Add LWW state tracking to follows, mutes, and blocks:

```sql
-- Follows: add state + timestamp for LWW-per-entry conflict resolution.
-- Instead of deleting rows on unfollow, set state = 'removed'.
ALTER TABLE follows ADD COLUMN state TEXT NOT NULL DEFAULT 'active';
ALTER TABLE follows ADD COLUMN last_changed_at INTEGER NOT NULL DEFAULT 0;

-- Mutes: same pattern.
ALTER TABLE mutes ADD COLUMN state TEXT NOT NULL DEFAULT 'active';
ALTER TABLE mutes ADD COLUMN last_changed_at INTEGER NOT NULL DEFAULT 0;

-- Blocks: same pattern.
ALTER TABLE blocks ADD COLUMN state TEXT NOT NULL DEFAULT 'active';
ALTER TABLE blocks ADD COLUMN last_changed_at INTEGER NOT NULL DEFAULT 0;
```

Add `device_pubkey`, `signature`, and `updated_at` columns to the profiles table. The `device_pubkey` and `signature` columns support profile signing. The `updated_at` column tracks when the profile was last modified, which is needed for device sync LWW conflict resolution (`DeviceSyncVector.profile_updated_at`):

```sql
ALTER TABLE profiles ADD COLUMN device_pubkey TEXT NOT NULL DEFAULT '';
ALTER TABLE profiles ADD COLUMN signature TEXT NOT NULL DEFAULT '';
ALTER TABLE profiles ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
```

Add author to bookmarks (pre-existing issue -- posts have compound PK `(author, id)`):

```sql
ALTER TABLE bookmarks ADD COLUMN author TEXT NOT NULL DEFAULT '';
```

Re-key DM ratchet sessions by device pubkey instead of identity pubkey. Drop and recreate (existing sessions will be re-established automatically via Noise IK handshake on next DM exchange):

```sql
DROP TABLE IF EXISTS dm_ratchet_sessions;
CREATE TABLE IF NOT EXISTS dm_ratchet_sessions (
    device_pubkey TEXT PRIMARY KEY,  -- was: peer_pubkey (identity)
    state_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
```

Update DM outbox for per-device delivery. Each target device gets its own outbox entry with a separately encrypted envelope (different ratchet session = different ciphertext):

```sql
-- peer_pubkey now means "target device pubkey" (not identity pubkey).
-- Add peer_identity for conversation grouping and cleanup.
ALTER TABLE dm_outbox ADD COLUMN peer_identity TEXT NOT NULL DEFAULT '';
```

When sending a DM to a peer with N devices, create N outbox entries. Mark the `dm_messages` row as delivered when any device receives it. Continue retrying remaining outbox entries for other devices independently.

### Storage Methods

```rust
// Device management (own devices)
fn save_device_info(info: &DeviceInfo) -> Result<()>
fn get_device_info() -> Result<Option<DeviceInfo>>
fn add_linked_device(device: &LinkedDevice) -> Result<()>
fn remove_linked_device(device_pubkey: &str) -> Result<()>
fn get_linked_devices() -> Result<Vec<LinkedDevice>>
fn update_device_last_seen(device_pubkey: &str, timestamp: u64) -> Result<()>

// Peer device certificates
fn cache_peer_devices(announcement: &LinkedDevicesAnnouncement) -> Result<()>
fn get_peer_device_certificate(identity: &str, device: &str) -> Result<Option<DeviceCertificate>>
fn get_peer_devices(identity: &str) -> Result<Vec<DeviceAnnouncement>>
fn is_device_revoked(identity: &str, device: &str) -> Result<bool>
fn cache_device_revocation(revocation: &DeviceRevocation) -> Result<()>

// Sync state
fn get_device_sync_state(device_pubkey: &str) -> Result<Option<DeviceSyncVector>>
fn update_device_sync_state(device_pubkey: &str, vector: &DeviceSyncVector) -> Result<()>

// Data export for pairing
fn export_link_bundle(certificate: &DeviceCertificate) -> Result<LinkBundle>
fn import_link_bundle(bundle: &LinkBundle) -> Result<()>
```

---

## Client Integration

### New Types (`crates/iroh-social-types/src/devices.rs`)

```rust
pub const LINK_ALPN: &[u8] = b"iroh-social/link/1";
pub const DEVICE_SYNC_ALPN: &[u8] = b"iroh-social/device-sync/1";
pub const CAP_ALL: u32      = 0xFFFF;

pub struct DeviceCertificate { ... }
pub struct LinkedDevicesAnnouncement { ... }
pub struct DeviceAnnouncement { ... }
pub struct DeviceRevocation { ... }
pub struct LinkQrPayload { ... }
pub struct LinkBundle { ... }
pub struct DeviceSyncVector { ... }
```

### Protocol Handlers

Register two new protocol handlers in the router:

```rust
let router = Router::builder(endpoint.clone())
    .accept(iroh_blobs::ALPN, blobs.clone())
    .accept(iroh_gossip::ALPN, gossip.clone())
    .accept(SYNC_ALPN, sync_handler)
    .accept(DM_ALPN, dm_handler.clone())
    .accept(LINK_ALPN, link_handler)             // NEW
    .accept(DEVICE_SYNC_ALPN, device_sync_handler) // NEW
    .spawn();
```

**LinkHandler**: Accepts incoming pairing connections from secondary devices. Performs Noise IK + PSK handshake, receives device pubkey, issues certificate, sends LinkBundle.

**DeviceSyncHandler**: Accepts incoming sync connections from linked devices. Verifies device authorization (certificate or identity key). Exchanges sync vectors and streams deltas.

### SyncHandler Update (Peer Sync)

The existing `SyncHandler` (for peer sync on `SYNC_ALPN`) must be updated to accept sync requests where `SyncRequest.author` matches this device's **linked identity pubkey**, not just its own NodeId. A secondary device's NodeId differs from the identity pubkey, but it stores posts with `author: <identity_pubkey>` (received via device sync). This allows peers and community servers to sync a user's posts from any of their devices as a fallback when the primary is offline.

### Tauri Commands

```
// Pairing
start_device_link()                -> LinkQrPayload  // primary: generates QR, starts listening
cancel_device_link()               -> ()
link_with_primary(qr_payload)      -> DeviceCertificate  // secondary: scans QR, pairs

// Device management
get_linked_devices()               -> Vec<LinkedDevice>
get_device_info()                  -> DeviceInfo  // this device's info
unlink_device(device_pubkey)       -> ()          // primary only
rename_device(device_pubkey, name) -> ()
is_primary_device()                -> bool

// Sync
force_device_sync()                -> { synced_items: u32 }
```

### Tauri Events

```
device-link-started    { qr_uri: String }            // QR code ready to display
device-link-progress   { step: String }               // pairing progress updates
device-linked          { device: LinkedDevice }        // pairing complete
device-unlinked        { device_pubkey: String }       // device removed
device-sync-complete   { device_pubkey, items: u32 }   // sync finished
device-revoked         { identity_pubkey, device }     // a peer's device was revoked
```

### Frontend Pages

**`/settings/devices` page:**

- Shows this device's info (name, pubkey, primary/secondary badge)
- Lists all linked devices (name, pubkey, last seen, primary/secondary badge)
- "Link New Device" button (primary only) -- opens QR code modal
- "Link to Primary" button (for new/unlinked devices) -- opens scanner/paste modal
- Unlink button per device (primary only)
- Rename device inline edit
- Force sync button

**QR Code Display (Primary):**

- Full-screen QR code modal with countdown timer (60s expiry)
- Pairing code displayed as text below QR (for desktop-to-desktop)
- "Waiting for secondary to scan..." status
- Cancel button

**QR Scanner (Secondary):**

- Camera viewfinder for scanning (reuses existing `ScannerModal.svelte`)
- Text input field for pasting pairing code
- Progress indicator during pairing
- Success/error states

### TypeScript Types

```typescript
interface DeviceCertificate {
  version: number;
  identity_pubkey: string;
  device_pubkey: string;
  device_name: string;
  issued_at: number;
  expires_at: number;
  capabilities: number;
  signature: string;
}

interface LinkedDevice {
  device_pubkey: string;
  device_name: string;
  is_primary: boolean;
  certificate: DeviceCertificate | null;
  last_seen_at: number;
}

interface DeviceInfo {
  device_pubkey: string;
  identity_pubkey: string;
  device_name: string;
  is_primary: boolean;
}
```

---

## Community Server Considerations

The community server (see `todos/community-server.md`) is an aggregation overlay that subscribes to users' gossip topics and indexes their posts. Linked devices affect the server in several ways:

### Server Must Handle Device Gossip Variants

The server's gossip subscriber receives all `GossipMessage` variants, including `LinkedDevices(LinkedDevicesAnnouncement)` and `DeviceRevocation(DeviceRevocation)`. It must:

1. Verify and cache device certificates from announcements (same logic as peer clients).
2. Cache revocations and reject posts from revoked devices.
3. Store certificates in a `peer_device_certificates` table and revocations in a `device_revocations` table.

### Server Must Verify Device-Signed Posts

When the server ingests a post with `device_pubkey != author`, it performs the same two-step verification chain as peer clients:

1. Verify content signature against the device key.
2. Look up the device's certificate and verify it was signed by the identity key, is not expired, and is not revoked.

If the certificate is not yet cached (e.g., the server started after the announcement was published), the server rejects the post and logs a warning. The certificate will arrive via gossip or sync, and future posts from that device will verify normally.

### Registration Remains Identity-Key-Only

Server registration requires the identity key signature, so only the primary device can register or unregister with a community server. Secondary devices inherit the registration -- the server ingests and verifies posts from any authorized device of a registered identity.

### Post and Interaction Schema

The server's `posts` and `interactions` tables include `device_pubkey` and `signature` columns to support device-aware verification and to preserve provenance information.

---

## Implementation Roadmap

### Phase 1: Types and Certificates

- [ ] Make `signature_to_hex()` and `hex_to_signature()` in `signing.rs` public (needed by `devices.rs` and `registration.rs`)
- [ ] Define `DeviceCertificate` type with canonical signing bytes
- [ ] Implement certificate creation (primary signs for secondary's pubkey)
- [ ] Implement `pub fn verify_certificate()` (check signature, expiry, capabilities)
- [ ] Define `LinkedDevicesAnnouncement` and `DeviceRevocation` types
- [ ] Add `device_pubkey` field to `Post`, `Interaction`, and `Profile` types
- [ ] Add `signature` and `updated_at` fields to `Profile` type
- [ ] Update `post_signing_bytes`, `interaction_signing_bytes` to include `device_pubkey`
- [ ] Implement `profile_signing_bytes`, `sign_profile`, `verify_profile_signature`
- [ ] Implement `delete_post_signing_bytes`, `delete_interaction_signing_bytes` and corresponding `sign_delete_post`/`verify_delete_post_signature`/`sign_delete_interaction`/`verify_delete_interaction_signature` functions
- [ ] Write certificate signing/verification unit tests

### Phase 2: Signing and Verification Changes

- [ ] Update `sign_post` / `sign_interaction` / `sign_profile` to use device key (not identity key)
- [ ] Update `verify_post_signature` / `verify_interaction_signature` / `verify_profile_signature` for two-step chain verification (including empty `device_pubkey` fallback)
- [ ] Add certificate lookup function (from cached announcements)
- [ ] Update gossip message validation in `FeedManager` to use new verification (including profile updates and signed deletes)
- [ ] Update sync-received profile verification: verify `SyncSummary.profile` via `verify_profile_signature()` with `SyncRequest.author` as `expected_identity`
- [ ] Add database migration for `device_pubkey` column on posts, interactions, and profiles (also `signature` and `updated_at` on profiles)
- [ ] Re-sign own profile, posts, and interactions with new signing bytes during migration (existing signatures are invalid because `device_pubkey` was added to canonical JSON)
- [ ] Backfill `device_pubkey = author` for peers' cached posts/interactions
- [ ] Write verification tests (primary posts, secondary posts, expired certs, revoked devices, profile updates, signed deletes, empty device_pubkey fallback)

### Phase 3: Storage and Device Management

- [ ] Add database migration for device tables (`device_info`, `linked_devices`, `peer_device_certificates`, `device_sync_state`, `device_revocations`)
- [ ] Implement storage methods for device management
- [ ] Implement storage methods for peer device certificate caching
- [ ] Implement `export_link_bundle` / `import_link_bundle`
- [ ] Add `LinkedDevices` and `DeviceRevocation` variants to `GossipMessage`
- [ ] Add `device_pubkey` and `signature` fields to `DeletePost` and `DeleteInteraction` gossip variants
- [ ] Add `DeviceAnnouncements` variant to `SyncFrame` and bump `SYNC_ALPN` to `b"iroh-social/sync/4"` (send announcements before posts/interactions)
- [ ] Handle incoming gossip announcements: validate and cache device info
- [ ] Update `SyncHandler` on secondary devices to accept sync requests where `SyncRequest.author` matches the linked identity pubkey (not just its own NodeId)
- [ ] Write storage integration tests

### Phase 4: Pairing Protocol

- [ ] Define `LINK_ALPN` and wire types (`LinkQrPayload`, `LinkBundle`)
- [ ] Implement `LinkHandler` (ProtocolHandler for pairing)
- [ ] Implement Noise IK + PSK handshake for pairing channel
- [ ] Implement primary side: generate QR, listen for connection, issue certificate, send bundle
- [ ] Implement secondary side: scan QR, connect, send device pubkey, receive bundle, import data
- [ ] Publish `LinkedDevicesAnnouncement` after successful pairing
- [ ] Add Tauri commands: `start_device_link`, `link_with_primary`, `cancel_device_link`
- [ ] Build QR code display modal (primary side)
- [ ] Build QR scan / paste modal (secondary side)
- [ ] Build `/settings/devices` management page
- [ ] Write pairing integration tests

### Phase 5: DM Multi-Device

- [ ] Bump `DM_ALPN` to `b"iroh-social/dm/2"` (breaking wire format change)
- [ ] Update `DmHandshake` to include `identity_pubkey` and `device_certificate` on both `Init` and `Response`
- [ ] Clarify `EncryptedEnvelope.sender` remains the identity pubkey (not device pubkey)
- [ ] Implement post-handshake identity verification (both sides verify device authorization)
- [ ] Update DM session establishment to use per-device X25519 keys
- [ ] Implement multi-device DM sending (encrypt for each recipient device)
- [ ] Drop and recreate `dm_ratchet_sessions` table keyed by `device_pubkey`
- [ ] Update `dm_outbox`: add `peer_identity` column, create N entries per message (one per target device)
- [ ] Implement peer device lookup for DM targeting (from cached announcements)
- [ ] Handle case where some devices are offline (send to online devices, queue offline in outbox)
- [ ] Write multi-device DM tests

### Phase 6: Device Sync

- [ ] Define `DEVICE_SYNC_ALPN` and sync vector types (including post/interaction count and timestamps)
- [ ] Implement `DeviceSyncHandler` (ProtocolHandler)
- [ ] Implement sync vector generation from local state
- [ ] Implement delta computation and streaming
- [ ] Implement post and interaction sync between own devices (delta by timestamp)
- [ ] Add LWW-per-entry state columns to `follows`, `mutes`, `blocks` tables
- [ ] Update follow/mute/block operations to set state + timestamp instead of deleting rows
- [ ] Implement LWW-per-entry merge logic for follows, mutes, blocks during sync
- [ ] Add `author` column to `bookmarks` table
- [ ] Add periodic sync task (60s interval)
- [ ] Add Tauri command: `force_device_sync`
- [ ] Write device sync tests

### Phase 7: Revocation and Polish

- [ ] Implement device revocation (primary signs and publishes)
- [ ] Handle incoming revocations: cache revocation, reject future posts from revoked devices
- [ ] Implement retroactive cleanup: delete cached posts/interactions from revoked device with timestamps after revocation
- [ ] Update DM sender to skip revoked devices
- [ ] Delete pending outbox entries targeting revoked devices on revocation receipt
- [ ] Secondary device revocation UX (notification, data wipe option)
- [ ] Certificate renewal (primary can issue new certificate for a device)
- [ ] Sync progress UI (show what's syncing, how much remains)
- [ ] Error states and edge cases (pairing timeout, sync failure, network interruption)
