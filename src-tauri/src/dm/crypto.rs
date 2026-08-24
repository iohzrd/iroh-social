use crate::crypto::RatchetHeader;
use crate::error::AppError;
use base64::Engine as _;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce, aead::Aead};
use proscenium_types::RatchetHeaderWire;

/// Encrypt a ratchet state JSON string with ChaCha20Poly1305.
/// Returns base64(nonce || ciphertext).
pub(crate) fn seal_ratchet_state(key: &[u8; 32], plaintext: &str) -> Result<String, AppError> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let mut nonce_bytes = [0u8; 12];
    getrandom::fill(&mut nonce_bytes).expect("getrandom failed");
    let nonce = Nonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| AppError::Other("ratchet state encryption failed".into()))?;
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(&blob))
}

/// Decrypt a ratchet state stored by `seal_ratchet_state`.
pub(crate) fn open_ratchet_state(key: &[u8; 32], stored: &str) -> Result<String, AppError> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .map_err(|_| AppError::Other("invalid base64 in ratchet state".into()))?;
    if blob.len() <= 12 {
        return Err(AppError::Other("ratchet state too short".into()));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let nonce = Nonce::try_from(nonce_bytes)
        .map_err(|_| AppError::Other("ratchet state nonce has invalid length".into()))?;
    let plaintext_bytes = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| AppError::Other("ratchet state decryption failed".into()))?;
    String::from_utf8(plaintext_bytes)
        .map_err(|_| AppError::Other("ratchet state not valid UTF-8".into()))
}

/// Decode a hex-encoded DM pubkey (X25519) to 32 bytes.
pub(crate) fn dm_pubkey_to_x25519(hex_pubkey: &str) -> Result<[u8; 32], AppError> {
    let bytes = hex::decode(hex_pubkey)?;
    bytes
        .try_into()
        .map_err(|_| AppError::Other("DM pubkey wrong length".into()))
}

pub(crate) fn ratchet_header_to_wire(header: &RatchetHeader) -> RatchetHeaderWire {
    RatchetHeaderWire {
        dh_public: hex::encode(header.dh_public),
        message_number: header.message_number,
        previous_chain_length: header.previous_chain_length,
    }
}

pub(crate) fn wire_to_ratchet_header(wire: &RatchetHeaderWire) -> Result<RatchetHeader, AppError> {
    let bytes = hex::decode(&wire.dh_public)?;
    let dh_public: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AppError::Other("invalid dh_public length".into()))?;
    Ok(RatchetHeader {
        dh_public,
        message_number: wire.message_number,
        previous_chain_length: wire.previous_chain_length,
    })
}

/// Encrypt a payload with Double Ratchet and save the updated ratchet state.
/// Returns the sealed envelope ready for transmission.
pub(super) async fn encrypt_and_save(
    handler: &super::DmHandler,
    peer_pubkey: &str,
    payload: &proscenium_types::DmPayload,
) -> Result<proscenium_types::EncryptedEnvelope, AppError> {
    let (mut ratchet, peer_dm_pubkey) =
        super::handshake::get_or_establish_session(handler, peer_pubkey).await?;

    let plaintext = serde_json::to_vec(payload)?;
    let (header, ciphertext) = ratchet.encrypt(&plaintext);

    let (dm_pubkey, ratchet_storage_key) = {
        let id = handler.identity.read().await;
        (id.dm_pubkey.clone(), id.ratchet_storage_key)
    };

    let ratchet_json = serde_json::to_string(&ratchet)?;
    let ratchet_sealed = seal_ratchet_state(&ratchet_storage_key, &ratchet_json)?;
    handler
        .storage
        .save_ratchet_session(
            &peer_dm_pubkey,
            &ratchet_sealed,
            proscenium_types::now_millis(),
        )
        .await?;

    let envelope = proscenium_types::EncryptedEnvelope {
        sender: dm_pubkey,
        ratchet_header: ratchet_header_to_wire(&header),
        ciphertext,
    };
    log::debug!(
        "[dm] encrypted envelope: sender={} msg_n={} prev_chain={} dh={} ciphertext_len={}",
        proscenium_types::short_id(&envelope.sender),
        envelope.ratchet_header.message_number,
        envelope.ratchet_header.previous_chain_length,
        &envelope.ratchet_header.dh_public[..8],
        envelope.ciphertext.len(),
    );

    Ok(envelope)
}
