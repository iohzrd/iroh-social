use crate::crypto::{
    RatchetHeader, RatchetState, ed25519_public_to_x25519, ed25519_secret_to_x25519,
    noise_complete_initiator, noise_complete_responder, noise_initiate, noise_respond,
    x25519_public_from_private,
};
use crate::storage::Storage;
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_social_types::{
    DM_ALPN, DirectMessage, DmHandshake, DmPayload, EncryptedEnvelope, RatchetHeaderWire,
    StoredMessage, now_millis, short_id,
};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone)]
pub struct DmHandler {
    storage: Arc<Storage>,
    app_handle: AppHandle,
    my_x25519_private: [u8; 32],
    my_x25519_public: [u8; 32],
    my_pubkey_str: String,
}

impl DmHandler {
    pub fn new(
        storage: Arc<Storage>,
        app_handle: AppHandle,
        ed25519_secret: [u8; 32],
        my_pubkey_str: String,
    ) -> Self {
        let my_x25519_private = ed25519_secret_to_x25519(&ed25519_secret);
        let my_x25519_public = x25519_public_from_private(&my_x25519_private);
        Self {
            storage,
            app_handle,
            my_x25519_private,
            my_x25519_public,
            my_pubkey_str,
        }
    }

    /// Get or establish a ratchet session with a peer.
    /// If no session exists, initiates a Noise IK handshake.
    async fn get_or_establish_session(
        &self,
        endpoint: &Endpoint,
        peer_pubkey: &str,
    ) -> anyhow::Result<RatchetState> {
        log::info!(
            "[dm] get_or_establish_session: peer={}",
            short_id(peer_pubkey)
        );

        // Try loading existing session
        if let Some(json) = self.storage.get_ratchet_session(peer_pubkey)? {
            log::info!(
                "[dm] loaded existing ratchet session for {} ({} bytes)",
                short_id(peer_pubkey),
                json.len()
            );
            let state: RatchetState = serde_json::from_str(&json)?;
            return Ok(state);
        }

        // No session -- need to handshake
        log::info!(
            "[dm] no existing session, initiating Noise IK handshake with {}",
            short_id(peer_pubkey)
        );

        let peer_id: EndpointId = peer_pubkey.parse().map_err(|e| {
            log::error!("[dm] failed to parse peer pubkey: {e}");
            anyhow::anyhow!("invalid peer pubkey: {e}")
        })?;
        let peer_ed_public = peer_id.as_bytes();
        let peer_x25519_public = ed25519_public_to_x25519(peer_ed_public)
            .ok_or_else(|| anyhow::anyhow!("invalid peer public key"))?;
        log::info!("[dm] converted peer ed25519 -> x25519 key");

        // Noise IK handshake: initiator
        let (initiator_hs, msg1) = noise_initiate(&self.my_x25519_private, &peer_x25519_public)
            .map_err(|e| anyhow::anyhow!("noise init: {e}"))?;
        log::info!("[dm] noise init message created ({} bytes)", msg1.len());

        // Connect and perform handshake
        let addr = EndpointAddr::from(peer_id);
        log::info!("[dm] connecting to {} on DM_ALPN...", short_id(peer_pubkey));
        let conn = endpoint.connect(addr, DM_ALPN).await.map_err(|e| {
            log::error!("[dm] QUIC connect failed to {}: {e}", short_id(peer_pubkey));
            e
        })?;
        log::info!("[dm] QUIC connected, opening bi-stream...");
        let (mut send, mut recv) = conn.open_bi().await?;

        // Send handshake init
        let handshake = DmHandshake::Init {
            noise_message: msg1,
        };
        let bytes = serde_json::to_vec(&handshake)?;
        log::info!("[dm] sending handshake init ({} bytes)...", bytes.len());
        send.write_all(&bytes).await?;
        send.finish()?;

        // Read handshake response
        log::info!("[dm] waiting for handshake response...");
        let resp_bytes = recv.read_to_end(65536).await?;
        log::info!(
            "[dm] received handshake response ({} bytes)",
            resp_bytes.len()
        );
        let resp: DmHandshake = serde_json::from_slice(&resp_bytes)?;

        let noise_response = match resp {
            DmHandshake::Response { noise_message } => noise_message,
            _ => return Err(anyhow::anyhow!("unexpected handshake response")),
        };

        // Complete handshake
        let shared_secret = noise_complete_initiator(initiator_hs, &noise_response)
            .map_err(|e| anyhow::anyhow!("noise complete: {e}"))?;
        log::info!("[dm] noise handshake completed successfully");

        conn.close(0u32.into(), b"done");

        // Initialize Double Ratchet as Alice (initiator)
        let ratchet = RatchetState::init_alice(&shared_secret, &peer_x25519_public);

        // Save session
        let json = serde_json::to_string(&ratchet)?;
        self.storage
            .save_ratchet_session(peer_pubkey, &json, now_millis())?;

        log::info!(
            "[dm] established and saved ratchet session with {}",
            short_id(peer_pubkey)
        );
        Ok(ratchet)
    }

    /// Send a DM to a peer. Encrypts with Double Ratchet and sends over QUIC.
    /// If the peer is offline, queues to outbox.
    /// On successful delivery, marks the message as delivered and emits `dm-delivered`.
    pub async fn send_dm(
        &self,
        endpoint: &Endpoint,
        peer_pubkey: &str,
        message: DirectMessage,
    ) -> anyhow::Result<()> {
        let message_id = message.id.clone();
        let mut ratchet = self.get_or_establish_session(endpoint, peer_pubkey).await?;

        // Encrypt the message
        let payload = DmPayload::Message(message);
        let plaintext = serde_json::to_vec(&payload)?;
        let (header, ciphertext) = ratchet.encrypt(&plaintext);

        // Save updated ratchet state
        let ratchet_json = serde_json::to_string(&ratchet)?;
        self.storage
            .save_ratchet_session(peer_pubkey, &ratchet_json, now_millis())?;

        let envelope = EncryptedEnvelope {
            sender: self.my_pubkey_str.clone(),
            ratchet_header: ratchet_header_to_wire(&header),
            ciphertext,
        };

        // Try to send
        match self
            .try_send_envelope(endpoint, peer_pubkey, &envelope)
            .await
        {
            Ok(()) => {
                log::info!("[dm] sent message to {}", short_id(peer_pubkey));
                self.mark_delivered(&message_id);
            }
            Err(e) => {
                log::warn!(
                    "[dm] peer {} offline, queuing to outbox: {e}",
                    short_id(peer_pubkey)
                );
                let envelope_json = serde_json::to_string(&envelope)?;
                let id = uuid::Uuid::new_v4().to_string();
                self.storage.insert_outbox_message(
                    &id,
                    peer_pubkey,
                    &envelope_json,
                    now_millis(),
                    &message_id,
                )?;
            }
        }

        Ok(())
    }

    /// Mark a message as delivered in storage and notify the frontend.
    fn mark_delivered(&self, message_id: &str) {
        if let Err(e) = self.storage.mark_dm_delivered(message_id) {
            log::error!(
                "[dm] failed to mark delivered {}: {e}",
                short_id(message_id)
            );
            return;
        }
        let _ = self.app_handle.emit(
            "dm-delivered",
            serde_json::json!({ "message_id": message_id }),
        );
    }

    /// Try to send an encrypted envelope to a peer over QUIC.
    async fn try_send_envelope(
        &self,
        endpoint: &Endpoint,
        peer_pubkey: &str,
        envelope: &EncryptedEnvelope,
    ) -> anyhow::Result<()> {
        let peer_id: EndpointId = peer_pubkey.parse()?;
        let addr = EndpointAddr::from(peer_id);

        let conn = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            endpoint.connect(addr, DM_ALPN),
        )
        .await
        .map_err(|_| anyhow::anyhow!("connection timeout"))??;

        let (mut send, mut recv) = conn.open_bi().await?;

        let bytes = serde_json::to_vec(envelope)?;
        send.write_all(&bytes).await?;
        send.finish()?;

        // Wait for ACK
        let ack = tokio::time::timeout(std::time::Duration::from_secs(5), recv.read_to_end(1024))
            .await
            .map_err(|_| anyhow::anyhow!("ack timeout"))??;

        if ack != b"ok" {
            return Err(anyhow::anyhow!("unexpected ack: {:?}", ack));
        }

        conn.close(0u32.into(), b"done");
        Ok(())
    }

    /// Flush all pending outbox messages for a peer.
    pub async fn flush_outbox_for_peer(
        &self,
        endpoint: &Endpoint,
        peer_pubkey: &str,
    ) -> anyhow::Result<(u32, u32)> {
        let entries = self.storage.get_outbox_for_peer(peer_pubkey)?;
        if entries.is_empty() {
            return Ok((0, 0));
        }

        let mut sent = 0u32;
        let mut failed = 0u32;

        for (id, envelope_json, message_id) in &entries {
            let envelope: EncryptedEnvelope = match serde_json::from_str(envelope_json) {
                Ok(e) => e,
                Err(_) => {
                    self.storage.remove_outbox_message(id)?;
                    failed += 1;
                    continue;
                }
            };

            match self
                .try_send_envelope(endpoint, peer_pubkey, &envelope)
                .await
            {
                Ok(()) => {
                    self.storage.remove_outbox_message(id)?;
                    self.mark_delivered(message_id);
                    sent += 1;
                }
                Err(_) => {
                    failed += 1;
                    // Stop trying this peer if first message fails (they're offline)
                    break;
                }
            }
        }

        if sent > 0 {
            log::info!(
                "[dm-outbox] flushed {sent} messages to {}",
                short_id(peer_pubkey)
            );
        }
        Ok((sent, failed))
    }

    /// Send a lightweight DM signal (typing, read receipt) without storing a message.
    pub async fn send_signal(
        &self,
        endpoint: &Endpoint,
        peer_pubkey: &str,
        payload: DmPayload,
    ) -> anyhow::Result<()> {
        let mut ratchet = self.get_or_establish_session(endpoint, peer_pubkey).await?;

        let plaintext = serde_json::to_vec(&payload)?;
        let (header, ciphertext) = ratchet.encrypt(&plaintext);

        let ratchet_json = serde_json::to_string(&ratchet)?;
        self.storage
            .save_ratchet_session(peer_pubkey, &ratchet_json, now_millis())?;

        let envelope = EncryptedEnvelope {
            sender: self.my_pubkey_str.clone(),
            ratchet_header: ratchet_header_to_wire(&header),
            ciphertext,
        };

        self.try_send_envelope(endpoint, peer_pubkey, &envelope)
            .await?;

        Ok(())
    }

    /// Handle an incoming handshake (Noise IK responder side).
    fn handle_handshake(
        &self,
        remote_pubkey: &str,
        noise_message: Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        log::info!("[dm] handling handshake from {}", short_id(remote_pubkey));

        // Noise IK responder
        let (responder_hs, response_msg) = noise_respond(&self.my_x25519_private, &noise_message)
            .map_err(|e| anyhow::anyhow!("noise respond: {e}"))?;

        let shared_secret = noise_complete_responder(responder_hs)
            .map_err(|e| anyhow::anyhow!("noise complete: {e}"))?;

        // Initialize Double Ratchet as Bob (responder)
        let ratchet = RatchetState::init_bob(
            &shared_secret,
            (self.my_x25519_private, self.my_x25519_public),
        );

        let json = serde_json::to_string(&ratchet)?;
        self.storage
            .save_ratchet_session(remote_pubkey, &json, now_millis())?;

        log::info!("[dm] session established with {}", short_id(remote_pubkey));

        let resp = DmHandshake::Response {
            noise_message: response_msg,
        };
        Ok(serde_json::to_vec(&resp)?)
    }

    /// Handle an incoming encrypted message.
    fn handle_encrypted_message(
        &self,
        remote_pubkey: &str,
        envelope: EncryptedEnvelope,
    ) -> anyhow::Result<()> {
        // Load ratchet session
        let json = self
            .storage
            .get_ratchet_session(remote_pubkey)?
            .ok_or_else(|| anyhow::anyhow!("no session with {}", short_id(remote_pubkey)))?;
        let mut ratchet: RatchetState = serde_json::from_str(&json)?;

        // Convert wire header to crypto header
        let header = wire_to_ratchet_header(&envelope.ratchet_header)?;

        // Decrypt
        let plaintext = ratchet
            .decrypt(&header, &envelope.ciphertext)
            .map_err(|e| anyhow::anyhow!("decrypt: {e}"))?;

        // Save updated ratchet state
        let ratchet_json = serde_json::to_string(&ratchet)?;
        self.storage
            .save_ratchet_session(remote_pubkey, &ratchet_json, now_millis())?;

        // Parse payload
        let payload: DmPayload = serde_json::from_slice(&plaintext)?;

        match payload {
            DmPayload::Message(msg) => {
                let conv_id = Storage::conversation_id(&self.my_pubkey_str, remote_pubkey);
                let preview = if msg.content.len() > 80 {
                    format!("{}...", &msg.content[..77])
                } else {
                    msg.content.clone()
                };

                let stored = StoredMessage {
                    id: msg.id.clone(),
                    conversation_id: conv_id.clone(),
                    from_pubkey: remote_pubkey.to_string(),
                    to_pubkey: self.my_pubkey_str.clone(),
                    content: msg.content,
                    timestamp: msg.timestamp,
                    media: msg.media,
                    read: false,
                    delivered: true,
                    reply_to: msg.reply_to,
                };

                // Conversation first (FK constraint), then message
                self.storage.upsert_conversation(
                    remote_pubkey,
                    &self.my_pubkey_str,
                    msg.timestamp,
                    &preview,
                )?;
                self.storage.insert_dm_message(&stored)?;
                self.storage.increment_unread(&conv_id)?;

                log::info!("[dm] received message from {}", short_id(remote_pubkey));

                let _ = self.app_handle.emit(
                    "dm-received",
                    serde_json::json!({
                        "from": remote_pubkey,
                        "message": stored,
                    }),
                );
            }
            DmPayload::Delivered { message_id } => {
                self.storage.mark_dm_delivered(&message_id)?;
                let _ = self.app_handle.emit(
                    "dm-delivered",
                    serde_json::json!({ "message_id": message_id }),
                );
            }
            DmPayload::Read { message_id } => {
                self.storage.mark_dm_read_by_id(&message_id)?;
                let _ = self
                    .app_handle
                    .emit("dm-read", serde_json::json!({ "message_id": message_id }));
            }
            DmPayload::Typing => {
                let _ = self.app_handle.emit(
                    "typing-indicator",
                    serde_json::json!({ "peer": remote_pubkey }),
                );
            }
        }

        Ok(())
    }
}

impl ProtocolHandler for DmHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let remote = conn.remote_id();
        let remote_str = remote.to_string();
        log::info!("[dm] incoming connection from {}", short_id(&remote_str));

        // Reject blocked peers
        if self.storage.is_blocked(&remote_str).unwrap_or(false) {
            log::warn!("[dm] rejecting blocked peer {}", short_id(&remote_str));
            return Err(AcceptError::from_err(std::io::Error::other("blocked")));
        }

        let (mut send, mut recv) = conn.accept_bi().await?;

        let frame_bytes = recv
            .read_to_end(1_048_576)
            .await
            .map_err(AcceptError::from_err)?;

        // Try handshake first, then encrypted message
        if let Ok(handshake) = serde_json::from_slice::<DmHandshake>(&frame_bytes) {
            match handshake {
                DmHandshake::Init { noise_message } => {
                    let response = self
                        .handle_handshake(&remote_str, noise_message)
                        .map_err(|e| AcceptError::from_err(std::io::Error::other(e)))?;
                    send.write_all(&response)
                        .await
                        .map_err(AcceptError::from_err)?;
                    send.finish().map_err(AcceptError::from_err)?;
                }
                DmHandshake::Response { .. } => {
                    // Should not receive a response as the server side
                    log::error!(
                        "[dm] unexpected handshake response from {}",
                        short_id(&remote_str)
                    );
                }
            }
        } else if let Ok(envelope) = serde_json::from_slice::<EncryptedEnvelope>(&frame_bytes) {
            if let Err(e) = self.handle_encrypted_message(&remote_str, envelope) {
                log::error!(
                    "[dm] failed to handle message from {}: {e}",
                    short_id(&remote_str)
                );
            }
            // Send ACK
            send.write_all(b"ok").await.map_err(AcceptError::from_err)?;
            send.finish().map_err(AcceptError::from_err)?;
        } else {
            log::error!("[dm] unknown frame from {}", short_id(&remote_str));
        }

        conn.closed().await;
        Ok(())
    }
}

// -- Helper functions for header conversion --

fn ratchet_header_to_wire(header: &RatchetHeader) -> RatchetHeaderWire {
    RatchetHeaderWire {
        dh_public: hex::encode(header.dh_public),
        message_number: header.message_number,
        previous_chain_length: header.previous_chain_length,
    }
}

fn wire_to_ratchet_header(wire: &RatchetHeaderWire) -> anyhow::Result<RatchetHeader> {
    let bytes = hex::decode(&wire.dh_public).map_err(|e| anyhow::anyhow!(e))?;
    let dh_public: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid dh_public length"))?;
    Ok(RatchetHeader {
        dh_public,
        message_number: wire.message_number,
        previous_chain_length: wire.previous_chain_length,
    })
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        if !s.len().is_multiple_of(2) {
            return Err("odd length hex string".to_string());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("invalid hex: {e}")))
            .collect()
    }
}
