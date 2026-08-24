use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce, aead::Aead};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use super::keys::{generate_x25519_keypair, x25519_dh};

/// Maximum number of skipped message keys to store.
const MAX_SKIP: u32 = 100;

/// HKDF info strings for KDF chain derivation.
const KDF_RK_INFO: &[u8] = b"proscenium-dm-rk";
const KDF_CK_INFO_KEY: &[u8] = b"proscenium-dm-ck-msg";
const KDF_CK_INFO_CHAIN: &[u8] = b"proscenium-dm-ck-chain";

#[derive(Debug, Clone)]
pub enum CryptoError {
    DecryptionFailed,
    TooManySkipped,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::DecryptionFailed => write!(f, "decryption failed"),
            CryptoError::TooManySkipped => write!(f, "too many skipped messages"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// A skipped message key for out-of-order delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedKey {
    pub ratchet_public: [u8; 32],
    pub message_number: u32,
    pub message_key: [u8; 32],
}

/// The ratchet header sent with each encrypted message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatchetHeader {
    pub dh_public: [u8; 32],
    pub message_number: u32,
    pub previous_chain_length: u32,
}

/// Persistent ratchet state for a conversation.
/// Serialized to JSON and stored in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatchetState {
    pub dh_self_private: [u8; 32],
    pub dh_self_public: [u8; 32],
    pub dh_remote_public: Option<[u8; 32]>,
    pub root_key: [u8; 32],
    pub chain_key_send: Option<[u8; 32]>,
    pub chain_key_recv: Option<[u8; 32]>,
    pub send_count: u32,
    pub recv_count: u32,
    pub prev_send_count: u32,
    pub skipped_keys: Vec<SkippedKey>,
}

/// KDF for root key ratchet: derives a new root key and chain key from
/// the current root key and a DH output.
fn kdf_rk(root_key: &[u8; 32], dh_output: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let hk = Hkdf::<sha2::Sha256>::new(Some(root_key), dh_output);
    let mut output = [0u8; 64];
    hk.expand(KDF_RK_INFO, &mut output)
        .expect("HKDF output length valid");
    let mut new_root = [0u8; 32];
    let mut chain_key = [0u8; 32];
    new_root.copy_from_slice(&output[..32]);
    chain_key.copy_from_slice(&output[32..]);
    output.zeroize();
    (new_root, chain_key)
}

/// KDF for chain key: derives the next chain key and a message key.
fn kdf_ck(chain_key: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let hk = Hkdf::<sha2::Sha256>::new(None, chain_key);
    let mut msg_key = [0u8; 32];
    hk.expand(KDF_CK_INFO_KEY, &mut msg_key)
        .expect("HKDF output length valid");
    let mut new_chain = [0u8; 32];
    hk.expand(KDF_CK_INFO_CHAIN, &mut new_chain)
        .expect("HKDF output length valid");
    (new_chain, msg_key)
}

/// Encrypt plaintext using a message key (ChaCha20Poly1305).
fn encrypt_message(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    // Use a zero nonce since each message key is unique (used exactly once).
    let nonce = Nonce::default();
    cipher
        .encrypt(&nonce, plaintext)
        .expect("encryption should not fail")
}

/// Decrypt ciphertext using a message key (ChaCha20Poly1305).
fn decrypt_message(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let nonce = Nonce::default();
    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| CryptoError::DecryptionFailed)
}

impl RatchetState {
    /// Initialize as the initiator (Alice) after Noise handshake.
    /// `shared_secret` is the Noise handshake hash.
    /// `bob_dh_public` is Bob's initial DH ratchet public key (his X25519 identity public).
    pub fn init_alice(shared_secret: &[u8; 32], bob_dh_public: &[u8; 32]) -> Self {
        let (dh_self_private, dh_self_public) = generate_x25519_keypair();
        let dh_output = x25519_dh(&dh_self_private, bob_dh_public);
        let (root_key, chain_key_send) = kdf_rk(shared_secret, &dh_output);

        RatchetState {
            dh_self_private,
            dh_self_public,
            dh_remote_public: Some(*bob_dh_public),
            root_key,
            chain_key_send: Some(chain_key_send),
            chain_key_recv: None,
            send_count: 0,
            recv_count: 0,
            prev_send_count: 0,
            skipped_keys: Vec::new(),
        }
    }

    /// Initialize as the responder (Bob) after Noise handshake.
    /// `shared_secret` is the Noise handshake hash.
    /// `bob_dh_keypair` is Bob's initial DH ratchet keypair (typically his X25519 identity key).
    pub fn init_bob(shared_secret: &[u8; 32], bob_dh_keypair: ([u8; 32], [u8; 32])) -> Self {
        RatchetState {
            dh_self_private: bob_dh_keypair.0,
            dh_self_public: bob_dh_keypair.1,
            dh_remote_public: None,
            root_key: *shared_secret,
            chain_key_send: None,
            chain_key_recv: None,
            send_count: 0,
            recv_count: 0,
            prev_send_count: 0,
            skipped_keys: Vec::new(),
        }
    }

    /// Encrypt a plaintext message. Returns the header and ciphertext.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> (RatchetHeader, Vec<u8>) {
        let chain_key = self
            .chain_key_send
            .expect("cannot encrypt without a sending chain key");
        let (new_chain, msg_key) = kdf_ck(&chain_key);
        self.chain_key_send = Some(new_chain);

        let header = RatchetHeader {
            dh_public: self.dh_self_public,
            message_number: self.send_count,
            previous_chain_length: self.prev_send_count,
        };
        self.send_count += 1;

        let ciphertext = encrypt_message(&msg_key, plaintext);
        (header, ciphertext)
    }

    /// Decrypt a received message.
    pub fn decrypt(
        &mut self,
        header: &RatchetHeader,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        // Try skipped message keys first
        if let Some(plaintext) = self.try_skipped_keys(header, ciphertext)? {
            return Ok(plaintext);
        }

        // Check if we need a DH ratchet step
        let need_dh_ratchet = self.dh_remote_public.as_ref() != Some(&header.dh_public);

        if need_dh_ratchet {
            self.skip_messages(header.previous_chain_length)?;
            self.dh_ratchet(&header.dh_public);
        }

        self.skip_messages(header.message_number)?;

        let chain_key = self.chain_key_recv.ok_or(CryptoError::DecryptionFailed)?;
        let (new_chain, msg_key) = kdf_ck(&chain_key);
        self.chain_key_recv = Some(new_chain);
        self.recv_count += 1;

        decrypt_message(&msg_key, ciphertext)
    }

    /// Try to decrypt using a skipped message key.
    fn try_skipped_keys(
        &mut self,
        header: &RatchetHeader,
        ciphertext: &[u8],
    ) -> Result<Option<Vec<u8>>, CryptoError> {
        let idx = self.skipped_keys.iter().position(|sk| {
            sk.ratchet_public == header.dh_public && sk.message_number == header.message_number
        });

        if let Some(idx) = idx {
            let sk = self.skipped_keys.remove(idx);
            let plaintext = decrypt_message(&sk.message_key, ciphertext)?;
            Ok(Some(plaintext))
        } else {
            Ok(None)
        }
    }

    /// Skip messages up to `until`, storing their message keys for later.
    fn skip_messages(&mut self, until: u32) -> Result<(), CryptoError> {
        if self.recv_count + MAX_SKIP < until {
            return Err(CryptoError::TooManySkipped);
        }

        if let Some(mut chain_key) = self.chain_key_recv {
            while self.recv_count < until {
                let (new_chain, msg_key) = kdf_ck(&chain_key);
                self.skipped_keys.push(SkippedKey {
                    ratchet_public: self.dh_remote_public.unwrap_or([0u8; 32]),
                    message_number: self.recv_count,
                    message_key: msg_key,
                });
                chain_key = new_chain;
                self.recv_count += 1;
            }
            self.chain_key_recv = Some(chain_key);
        }

        Ok(())
    }

    /// Perform a DH ratchet step when receiving a new DH public key.
    fn dh_ratchet(&mut self, new_remote_public: &[u8; 32]) {
        self.prev_send_count = self.send_count;
        self.send_count = 0;
        self.recv_count = 0;
        self.dh_remote_public = Some(*new_remote_public);

        // Derive receiving chain
        let dh_recv = x25519_dh(&self.dh_self_private, new_remote_public);
        let (root_key, chain_key_recv) = kdf_rk(&self.root_key, &dh_recv);
        self.root_key = root_key;
        self.chain_key_recv = Some(chain_key_recv);

        // Generate new DH keypair and derive sending chain
        let (new_private, new_public) = generate_x25519_keypair();
        self.dh_self_private = new_private;
        self.dh_self_public = new_public;

        let dh_send = x25519_dh(&self.dh_self_private, new_remote_public);
        let (root_key, chain_key_send) = kdf_rk(&self.root_key, &dh_send);
        self.root_key = root_key;
        self.chain_key_send = Some(chain_key_send);
    }
}
