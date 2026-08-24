use super::*;

#[test]
fn test_key_conversion_produces_valid_x25519() {
    let mut ed_secret = [0u8; 32];
    getrandom::fill(&mut ed_secret).unwrap();

    let x_private = ed25519_secret_to_x25519(&ed_secret);
    let x_public = x25519_public_from_private(&x_private);

    // Verify the key is clamped correctly
    assert_eq!(x_private[0] & 7, 0);
    assert_eq!(x_private[31] & 128, 0);
    assert_eq!(x_private[31] & 64, 64);

    // Verify public key is non-zero
    assert_ne!(x_public, [0u8; 32]);
}

#[test]
fn test_ed25519_public_to_x25519() {
    use sha2::{Digest, Sha512};

    let mut ed_secret = [0u8; 32];
    getrandom::fill(&mut ed_secret).unwrap();

    // Derive ed25519 public key using ed25519-dalek-compatible method:
    // The public key is the compressed Edwards Y coordinate of the scalar * basepoint.
    // We use curve25519_dalek directly since we have it as a dependency.
    let hash = Sha512::digest(ed_secret);
    let mut scalar_bytes = [0u8; 32];
    scalar_bytes.copy_from_slice(&hash[..32]);
    scalar_bytes[0] &= 248;
    scalar_bytes[31] &= 127;
    scalar_bytes[31] |= 64;

    use curve25519_dalek::edwards::EdwardsPoint;
    use curve25519_dalek::scalar::Scalar;
    let scalar = Scalar::from_bytes_mod_order(scalar_bytes);
    let point = EdwardsPoint::mul_base(&scalar);
    let ed_public = point.compress().to_bytes();

    let x_public = ed25519_public_to_x25519(&ed_public);
    assert!(x_public.is_some());

    // The X25519 public derived from the ed25519 public should match
    // the X25519 public derived from the ed25519 secret
    let x_private = ed25519_secret_to_x25519(&ed_secret);
    let x_public_from_private = x25519_public_from_private(&x_private);
    assert_eq!(x_public.unwrap(), x_public_from_private);
}

#[test]
fn test_noise_handshake() {
    let mut alice_ed = [0u8; 32];
    let mut bob_ed = [0u8; 32];
    getrandom::fill(&mut alice_ed).unwrap();
    getrandom::fill(&mut bob_ed).unwrap();

    let alice_x = ed25519_secret_to_x25519(&alice_ed);
    let bob_x = ed25519_secret_to_x25519(&bob_ed);
    let bob_x_pub = x25519_public_from_private(&bob_x);

    // Alice initiates (knows Bob's public key)
    let (alice_hs, msg1) = noise_initiate(&alice_x, &bob_x_pub).unwrap();

    // Bob responds
    let (bob_hs, msg2) = noise_respond(&bob_x, &msg1).unwrap();

    // Both complete and get the same handshake hash
    let alice_hash = noise_complete_initiator(alice_hs, &msg2).unwrap();
    let (bob_hash, _) = noise_complete_responder(bob_hs).unwrap();
    assert_eq!(alice_hash, bob_hash);
}

#[test]
fn test_ratchet_basic_encrypt_decrypt() {
    let shared_secret = [42u8; 32];
    let bob_x_private = {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    };
    let bob_x_public = x25519_public_from_private(&bob_x_private);

    let mut alice = RatchetState::init_alice(&shared_secret, &bob_x_public);
    let mut bob = RatchetState::init_bob(&shared_secret, (bob_x_private, bob_x_public));

    // Alice sends a message to Bob
    let plaintext = b"Hello Bob!";
    let (header, ciphertext) = alice.encrypt(plaintext);
    let decrypted = bob.decrypt(&header, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_ratchet_multiple_messages_same_direction() {
    let shared_secret = [42u8; 32];
    let bob_x_private = {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    };
    let bob_x_public = x25519_public_from_private(&bob_x_private);

    let mut alice = RatchetState::init_alice(&shared_secret, &bob_x_public);
    let mut bob = RatchetState::init_bob(&shared_secret, (bob_x_private, bob_x_public));

    for i in 0..5 {
        let msg = format!("Message {i}");
        let (header, ciphertext) = alice.encrypt(msg.as_bytes());
        let decrypted = bob.decrypt(&header, &ciphertext).unwrap();
        assert_eq!(decrypted, msg.as_bytes());
    }
}

#[test]
fn test_ratchet_alternating_directions() {
    let shared_secret = [42u8; 32];
    let bob_x_private = {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    };
    let bob_x_public = x25519_public_from_private(&bob_x_private);

    let mut alice = RatchetState::init_alice(&shared_secret, &bob_x_public);
    let mut bob = RatchetState::init_bob(&shared_secret, (bob_x_private, bob_x_public));

    // Alice -> Bob
    let (h, c) = alice.encrypt(b"Hi Bob");
    assert_eq!(bob.decrypt(&h, &c).unwrap(), b"Hi Bob");

    // Bob -> Alice
    let (h, c) = bob.encrypt(b"Hi Alice");
    assert_eq!(alice.decrypt(&h, &c).unwrap(), b"Hi Alice");

    // Alice -> Bob again
    let (h, c) = alice.encrypt(b"How are you?");
    assert_eq!(bob.decrypt(&h, &c).unwrap(), b"How are you?");

    // Bob -> Alice again
    let (h, c) = bob.encrypt(b"Good!");
    assert_eq!(alice.decrypt(&h, &c).unwrap(), b"Good!");
}

#[test]
fn test_ratchet_out_of_order_messages() {
    let shared_secret = [42u8; 32];
    let bob_x_private = {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    };
    let bob_x_public = x25519_public_from_private(&bob_x_private);

    let mut alice = RatchetState::init_alice(&shared_secret, &bob_x_public);
    let mut bob = RatchetState::init_bob(&shared_secret, (bob_x_private, bob_x_public));

    // Alice sends 3 messages
    let (h1, c1) = alice.encrypt(b"msg1");
    let (h2, c2) = alice.encrypt(b"msg2");
    let (h3, c3) = alice.encrypt(b"msg3");

    // Bob receives them out of order
    assert_eq!(bob.decrypt(&h3, &c3).unwrap(), b"msg3");
    assert_eq!(bob.decrypt(&h1, &c1).unwrap(), b"msg1");
    assert_eq!(bob.decrypt(&h2, &c2).unwrap(), b"msg2");
}

#[test]
fn test_ratchet_serialization() {
    let shared_secret = [42u8; 32];
    let bob_x_private = {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    };
    let bob_x_public = x25519_public_from_private(&bob_x_private);

    let mut alice = RatchetState::init_alice(&shared_secret, &bob_x_public);

    // Send a message to advance state
    let _ = alice.encrypt(b"test");

    // Serialize and deserialize
    let json = serde_json::to_string(&alice).unwrap();
    let restored: RatchetState = serde_json::from_str(&json).unwrap();

    assert_eq!(alice.dh_self_public, restored.dh_self_public);
    assert_eq!(alice.root_key, restored.root_key);
    assert_eq!(alice.send_count, restored.send_count);
}

#[test]
fn test_ratchet_wrong_key_fails() {
    let shared_secret = [42u8; 32];
    let bob_x_private = {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    };
    let bob_x_public = x25519_public_from_private(&bob_x_private);

    let mut alice = RatchetState::init_alice(&shared_secret, &bob_x_public);

    // Eve has a different shared secret
    let eve_private = {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    };
    let eve_public = x25519_public_from_private(&eve_private);
    let mut eve = RatchetState::init_bob(&[99u8; 32], (eve_private, eve_public));

    let (header, ciphertext) = alice.encrypt(b"secret message");
    assert!(eve.decrypt(&header, &ciphertext).is_err());
}

#[test]
fn test_full_noise_then_ratchet() {
    // Full integration: Noise handshake -> Double Ratchet conversation
    let mut alice_ed = [0u8; 32];
    let mut bob_ed = [0u8; 32];
    getrandom::fill(&mut alice_ed).unwrap();
    getrandom::fill(&mut bob_ed).unwrap();

    let alice_x = ed25519_secret_to_x25519(&alice_ed);
    let bob_x = ed25519_secret_to_x25519(&bob_ed);
    let bob_x_pub = x25519_public_from_private(&bob_x);

    // Noise handshake
    let (alice_hs, msg1) = noise_initiate(&alice_x, &bob_x_pub).unwrap();
    let (bob_hs, msg2) = noise_respond(&bob_x, &msg1).unwrap();
    let shared_secret = noise_complete_initiator(alice_hs, &msg2).unwrap();
    let (bob_shared, _) = noise_complete_responder(bob_hs).unwrap();
    assert_eq!(shared_secret, bob_shared);

    // Initialize ratchets
    // Bob uses his X25519 identity key as the initial ratchet key
    let mut alice = RatchetState::init_alice(&shared_secret, &bob_x_pub);
    let mut bob = RatchetState::init_bob(&shared_secret, (bob_x, bob_x_pub));

    // Conversation
    let (h, c) = alice.encrypt(b"Hello from Alice");
    assert_eq!(bob.decrypt(&h, &c).unwrap(), b"Hello from Alice");

    let (h, c) = bob.encrypt(b"Hello from Bob");
    assert_eq!(alice.decrypt(&h, &c).unwrap(), b"Hello from Bob");

    let (h, c) = alice.encrypt(b"This is E2E encrypted!");
    assert_eq!(bob.decrypt(&h, &c).unwrap(), b"This is E2E encrypted!");
}
