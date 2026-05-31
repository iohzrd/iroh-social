use super::auth;
use super::fanout::Fanout;
use super::mixer::MixerHandle;
use super::relay;
use super::speaker_mixer::SpeakerMixerHandle;
use super::state::SfuHub;
use super::{CONN_TYPE_LISTENER, CONN_TYPE_SPEAKER};
use crate::audio::{
    AudioCapture, AudioPlayback, EchoCanceller, OpusDecoder, OpusEncoder, SAMPLES_PER_FRAME,
    TAG_NORMAL, read_audio_frame, write_audio_frame,
};
use crate::error::AppError;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use proscenium_types::{STAGE_ALPN, StageEvent, StageRole, short_id};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---- Audio pipeline free functions --------------------------------------

/// Handle an incoming STAGE_ALPN connection based on our current role.
///
/// Called from a spawned task so it can do async I/O without blocking the actor.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_incoming_connection(
    conn: Connection,
    my_role: StageRole,
    fanout: Option<Arc<Fanout>>,
    mixer_handle: Option<MixerHandle>,
    relay_handle: Option<relay::RelayHandle>,
    sfu_hub: Option<Arc<tokio::sync::Mutex<SfuHub>>>,
    host_sfu_fanout: Option<Arc<Fanout>>,
    banned_node_ids: std::collections::HashSet<String>,
    cancel: CancellationToken,
) {
    let remote = conn.remote_id().to_string();

    if banned_node_ids.contains(&remote) {
        log::info!(
            "[stage] rejected connection from banned node {}",
            short_id(&remote)
        );
        conn.close(1u32.into(), b"banned");
        return;
    }

    // If we are acting as a relay, accept downstream listener connections.
    if let Some(relay_handle) = relay_handle {
        let (send, recv) =
            match tokio::time::timeout(std::time::Duration::from_secs(5), conn.accept_bi()).await {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    log::warn!(
                        "[stage-relay] failed to accept bi-stream from {}: {e}",
                        short_id(&remote)
                    );
                    return;
                }
                Err(_) => {
                    log::warn!(
                        "[stage-relay] timeout accepting bi-stream from {}",
                        short_id(&remote)
                    );
                    return;
                }
            };

        let mut type_buf = [0u8; 1];
        let mut recv = recv;
        if tokio::time::timeout(
            std::time::Duration::from_secs(5),
            recv.read_exact(&mut type_buf),
        )
        .await
        .is_err()
        {
            log::warn!(
                "[stage-relay] timeout reading conn type from {}",
                short_id(&remote)
            );
            return;
        }
        drop(recv);

        match type_buf[0] {
            CONN_TYPE_LISTENER => {
                log::info!(
                    "[stage-relay] downstream listener connected: {}",
                    short_id(&remote)
                );
                if let Err(e) = relay_handle.add_downstream(send).await {
                    log::warn!("[stage-relay] failed to add downstream listener: {e}");
                }
            }
            unknown => {
                log::warn!(
                    "[stage-relay] unexpected conn type {unknown:#x} from {}",
                    short_id(&remote)
                );
                conn.close(0u32.into(), b"relay only accepts listeners");
            }
        }
        return;
    }

    match my_role {
        StageRole::Host => {
            let (mut send, mut recv) =
                match tokio::time::timeout(std::time::Duration::from_secs(5), conn.accept_bi())
                    .await
                {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(e)) => {
                        log::warn!(
                            "[stage] failed to accept bi-stream from {}: {e}",
                            short_id(&remote)
                        );
                        return;
                    }
                    Err(_) => {
                        log::warn!(
                            "[stage] timeout accepting bi-stream from {}",
                            short_id(&remote)
                        );
                        return;
                    }
                };

            let mut type_buf = [0u8; 1];
            if tokio::time::timeout(
                std::time::Duration::from_secs(5),
                recv.read_exact(&mut type_buf),
            )
            .await
            .is_err()
            {
                log::warn!(
                    "[stage] timeout reading conn type from {}",
                    short_id(&remote)
                );
                return;
            }

            match type_buf[0] {
                CONN_TYPE_SPEAKER => {
                    log::info!("[stage-host] speaker connected: {}", short_id(&remote));
                    let _ = send.finish(); // no return bi-stream; host uses SFU uni-streams

                    if let (Some(mixer), Some(hub)) = (mixer_handle, sfu_hub) {
                        let (pcm_tx, pcm_rx) = mpsc::channel::<Vec<f32>>(32);
                        if mixer.add_speaker(remote.clone(), pcm_rx).await.is_err() {
                            return;
                        }

                        // Create this speaker's SFU fanout.
                        let sfu_fanout = Arc::new(Fanout::new());

                        // Atomically register in hub; snapshot existing peers.
                        let (snap_fanouts, snap_conns) = {
                            let mut locked = hub.lock().await;
                            let sf = locked.fanouts.clone();
                            let sc = locked.connections.clone();
                            locked.fanouts.insert(remote.clone(), sfu_fanout.clone());
                            locked.connections.insert(remote.clone(), conn.clone());
                            (sf, sc)
                        };

                        // Subscribe new speaker to each existing speaker's audio,
                        // and subscribe each existing speaker to the new speaker's audio.
                        for (e_id, e_fanout) in &snap_fanouts {
                            if let Ok(uni) = conn.open_uni().await {
                                e_fanout.add_subscriber(uni, &cancel);
                            }
                            if let Some(e_conn) = snap_conns.get(e_id)
                                && let Ok(uni) = e_conn.open_uni().await
                            {
                                sfu_fanout.add_subscriber(uni, &cancel);
                            }
                        }

                        // Subscribe new speaker to host's voice.
                        if let Some(hf) = host_sfu_fanout
                            && let Ok(uni) = conn.open_uni().await
                        {
                            hf.add_subscriber(uni, &cancel);
                        }

                        tokio::spawn(speaker_recv_sfu_loop(
                            recv, remote, pcm_tx, sfu_fanout, hub, cancel,
                        ));
                    }
                }
                CONN_TYPE_LISTENER => {
                    log::info!("[stage-host] listener connected: {}", short_id(&remote));
                    if let Some(f) = fanout {
                        f.add_subscriber(send, &cancel);
                    }
                }
                unknown => {
                    log::warn!(
                        "[stage] unknown conn type {unknown:#x} from {}",
                        short_id(&remote)
                    );
                    conn.close(0u32.into(), b"unknown connection type");
                }
            }
        }
        _ => {
            log::debug!(
                "[stage] rejecting incoming from {} (role {:?})",
                short_id(&remote),
                my_role
            );
            conn.close(0u32.into(), b"not accepting connections in this role");
        }
    }
}

/// Receive Opus frames from a speaker's QUIC stream.
/// Tees each raw frame to the speaker's SFU fanout (for forwarding to other speakers)
/// and decodes PCM to the mixer channel.
/// Removes the speaker from the SFU hub when the stream closes.
async fn speaker_recv_sfu_loop(
    mut recv: iroh::endpoint::RecvStream,
    node_id: String,
    pcm_tx: mpsc::Sender<Vec<f32>>,
    sfu_fanout: Arc<Fanout>,
    sfu_hub: Arc<tokio::sync::Mutex<SfuHub>>,
    cancel: CancellationToken,
) {
    let mut decoder = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!(
                "[stage-host] failed to create decoder for {}: {e}",
                short_id(&node_id)
            );
            return;
        }
    };

    let mut frames: u32 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = read_audio_frame(&mut recv) => {
                match result {
                    Ok(Some((seq, ts, tag, payload))) => {
                        // Decode first (borrows payload), then forward raw bytes.
                        let decoded = decoder.decode(&payload);
                        sfu_fanout.send_frame(seq, ts, tag, payload);
                        match decoded {
                            Ok(samples) => {
                                if pcm_tx.send(samples).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "[stage-host] decode error from {}: {e}",
                                    short_id(&node_id)
                                );
                            }
                        }
                        frames += 1;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!(
                            "[stage-host] recv error from {}: {e}",
                            short_id(&node_id)
                        );
                        break;
                    }
                }
            }
        }
    }

    // Clean up SFU hub entry so future speakers don't try to subscribe to a dead fanout.
    {
        let mut hub = sfu_hub.lock().await;
        hub.fanouts.remove(&node_id);
        hub.connections.remove(&node_id);
    }
    log::debug!(
        "[stage-host] speaker {} disconnected ({frames} frames), removed from SFU hub",
        short_id(&node_id)
    );
}

/// Connect to an upstream node (host or relay) as a listener and start relaying the
/// mixed audio stream to downstream subscribers via a `RelayActor`.
///
/// Returns the `RelayHandle` used to register downstream listener connections.
pub(super) async fn start_relay_pipeline(
    endpoint: Endpoint,
    upstream_node_id: String,
    cancel: CancellationToken,
) -> Result<relay::RelayHandle, AppError> {
    let upstream_id: EndpointId = upstream_node_id
        .parse()
        .map_err(|e| AppError::Other(format!("invalid relay upstream node id: {e}")))?;

    let conn = endpoint
        .connect(EndpointAddr::from(upstream_id), STAGE_ALPN)
        .await
        .map_err(|e| AppError::Other(format!("[stage-relay] connect to upstream failed: {e}")))?;

    let (mut send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| AppError::Other(format!("[stage-relay] open bi-stream failed: {e}")))?;

    // Identify ourselves as a listener to the upstream; drop send after (host doesn't use it further)
    send.write_all(&[CONN_TYPE_LISTENER])
        .await
        .map_err(|_| AppError::Other("[stage-relay] write conn type failed".into()))?;

    log::info!(
        "[stage-relay] connected to upstream {}, spawning relay actor",
        short_id(&upstream_node_id)
    );

    relay::spawn_relay(recv, cancel)
}

/// Receive mix-minus PCM from the mixer and play it locally (host only).
///
/// The mixer sends only the contributions of other speakers -- the host's own
/// voice is excluded, eliminating echo without a round-trip through the network.
pub(super) async fn run_host_playback(
    mut rx: mpsc::Receiver<Vec<f32>>,
    far_end_tx: mpsc::Sender<Vec<f32>>,
    cancel: CancellationToken,
) {
    let (mut prod, _playback) = match AudioPlayback::start(None) {
        Ok(p) => p,
        Err(e) => {
            log::error!("[stage-host] failed to start playback: {e}");
            return;
        }
    };

    let mut frames: u32 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            pcm = rx.recv() => {
                let Some(samples) = pcm else { break };
                let _ = far_end_tx.try_send(samples.clone());
                prod.push(&samples);
                frames += 1;
            }
        }
    }
    log::info!("[stage-host] host playback stopped ({frames} frames)");
}

/// Connect to an upstream node (host or relay), receive the mixed stream, verify auth,
/// decode, and play back. Automatically reconnects with exponential backoff on stream failure.
pub(super) async fn start_listener_pipeline(
    endpoint: Endpoint,
    upstream_node_id: String,
    host_signing_pubkey: String,
    app_handle: AppHandle,
    cancel: CancellationToken,
) {
    const BACKOFF_INITIAL: std::time::Duration = std::time::Duration::from_secs(2);
    const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);
    let mut backoff = BACKOFF_INITIAL;

    loop {
        run_listener_once(
            &endpoint,
            &upstream_node_id,
            &host_signing_pubkey,
            &app_handle,
            &cancel,
        )
        .await;

        if cancel.is_cancelled() {
            break;
        }

        log::info!(
            "[stage-listener] stream ended, reconnecting in {:?}",
            backoff
        );
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Single connection attempt for the listener audio pipeline.
async fn run_listener_once(
    endpoint: &Endpoint,
    host_node_id: &str,
    host_signing_pubkey: &str,
    app_handle: &AppHandle,
    cancel: &CancellationToken,
) {
    let host_id: EndpointId = match host_node_id.parse() {
        Ok(id) => id,
        Err(e) => {
            log::error!("[stage-listener] invalid host node id: {e}");
            return;
        }
    };

    let conn = match endpoint
        .connect(EndpointAddr::from(host_id), STAGE_ALPN)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            log::error!("[stage-listener] failed to connect to host: {e}");
            return;
        }
    };

    let source_id = conn.remote_id().to_string();

    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(pair) => pair,
        Err(e) => {
            log::error!("[stage-listener] failed to open bi-stream: {e}");
            return;
        }
    };

    // Identify ourselves as a listener
    if send.write_all(&[CONN_TYPE_LISTENER]).await.is_err() {
        return;
    }

    log::info!("[stage-listener] connected to host, starting playback");

    // Build listener auth state from host's signing public key
    let host_pubkey = match host_signing_pubkey.parse::<iroh::PublicKey>() {
        Ok(pk) => pk,
        Err(e) => {
            log::error!("[stage-listener] invalid host pubkey: {e}");
            return;
        }
    };
    let mut auth = auth::ListenerAuthState::new(host_pubkey);

    let mut decoder = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!("[stage-listener] failed to create decoder: {e}");
            return;
        }
    };

    // Adaptive jitter buffer.
    //
    // AudioPlayback::start(None) owns ring buffer creation and the cpal stream.
    // PlaybackProducer is the sole write path; the cpal callback is the sole
    // reader. Capacity is PLAYBACK_CAPACITY_FRAMES (defined in playback.rs).
    //
    // Adaptation rules (applied once per decoded frame while playing):
    //   - Underrun detected  -> target += 1 (up to JB_MAX_FRAMES)
    //   - DRIFT_INTERVAL consecutive frames with no underrun -> target -= 1 (down to JB_MIN_FRAMES)
    //
    // The target governs the pre-fill gate: playback (re-)starts only once
    // target_frames decoded frames are buffered, so the callback never fires
    // into an empty buffer. Depth converges toward the minimum that avoids
    // underruns on the current network path.
    const JB_MIN_FRAMES: usize = 3; // 60 ms -- LAN / low-latency WAN
    const JB_INIT_FRAMES: usize = 4; // 80 ms -- conservative start
    const JB_MAX_FRAMES: usize = 10; // 200 ms -- ceiling for bad WAN
    const DRIFT_INTERVAL: usize = 250; // ~5 s at 20 ms/frame before drifting down

    // cpal starts immediately (outputs silence until pre-fill completes).
    let (mut prod, playback) = match AudioPlayback::start(None) {
        Ok(p) => p,
        Err(e) => {
            log::error!("[stage-listener] failed to start playback: {e}");
            return;
        }
    };

    let mut playback_live = false;
    let mut prefill_count: usize = 0;

    let mut target_frames: usize = JB_INIT_FRAMES;
    let mut frames_since_adapt: usize = 0;

    // Sequence tracking for packet loss concealment.
    let mut last_seq: Option<u32> = None;

    let mut frames: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = read_audio_frame(&mut recv) => {
                match result {
                    Ok(Some((seq, _, tag, wire_payload))) => {
                        // Fill any detected gaps with PLC frames before processing this frame.
                        if let Some(prev) = last_seq {
                            let lost = seq.wrapping_sub(prev.wrapping_add(1));
                            if lost > 0 && lost < 64 {
                                log::warn!("[stage-listener] {lost} lost frame(s) before seq {seq}, inserting PLC");
                                for _ in 0..lost {
                                    if let Ok(plc) = decoder.decode_loss() {
                                        prod.push(&plc);
                                    }
                                }
                            }
                        }
                        last_seq = Some(seq);

                        // Split checkpoint prefix from Opus payload.
                        let (checkpoint, opus) =
                            if tag == crate::audio::transport::TAG_CHECKPOINT {
                                match auth::decode_checkpoint_payload(&wire_payload) {
                                    Ok((hash, sig, opus)) => (Some((hash, sig)), opus),
                                    Err(e) => {
                                        log::warn!("[stage-listener] malformed checkpoint: {e}");
                                        continue;
                                    }
                                }
                            } else {
                                (None, wire_payload.as_slice())
                            };

                        // Verify auth chain. TamperDetected and InvalidSignature
                        // are fatal -- the auth state is permanently compromised
                        // and we must disconnect.
                        let auth_result = auth.verify_frame(opus, tag, checkpoint);
                        match &auth_result {
                            auth::AuthResult::TamperDetected => {
                                log::warn!("[stage-listener] tamper detected from {}, disconnecting", short_id(&source_id));
                                let _ = app_handle.emit(
                                    "stage-event",
                                    StageEvent::AuthFailed {
                                        source: source_id.clone(),
                                        reason: "tamper_detected".to_string(),
                                    },
                                );
                                break;
                            }
                            auth::AuthResult::InvalidSignature => {
                                log::warn!("[stage-listener] invalid signature from {}, disconnecting", short_id(&source_id));
                                let _ = app_handle.emit(
                                    "stage-event",
                                    StageEvent::AuthFailed {
                                        source: source_id.clone(),
                                        reason: "invalid_signature".to_string(),
                                    },
                                );
                                break;
                            }
                            _ => {}
                        }

                        // Decode and push into jitter buffer.
                        if let Ok(samples) = decoder.decode(opus) {
                            let pushed = prod.push(&samples);
                            if pushed < samples.len() {
                                log::debug!(
                                    "[stage-listener] jitter buffer full, dropped {} samples",
                                    samples.len() - pushed
                                );
                            }
                            frames += 1;

                            if playback_live {
                                // Adaptive depth: check for underruns each frame.
                                let underruns = prod.drain_underruns();
                                if underruns > 0 {
                                    if target_frames < JB_MAX_FRAMES {
                                        target_frames += 1;
                                        log::debug!(
                                            "[stage-listener] underrun x{underruns}, target -> {}f ({}ms)",
                                            target_frames,
                                            target_frames * 20
                                        );
                                    }
                                    frames_since_adapt = 0;
                                } else {
                                    frames_since_adapt += 1;
                                    if frames_since_adapt >= DRIFT_INTERVAL
                                        && target_frames > JB_MIN_FRAMES
                                    {
                                        target_frames -= 1;
                                        frames_since_adapt = 0;
                                        log::debug!(
                                            "[stage-listener] drifting down, target -> {}f ({}ms)",
                                            target_frames,
                                            target_frames * 20
                                        );
                                    }
                                }
                            } else {
                                // Pre-fill gate: begin adaptive tracking once target depth is buffered.
                                prefill_count += 1;
                                if prefill_count >= target_frames {
                                    log::info!(
                                        "[stage-listener] pre-fill reached ({}f / {}ms), audio live",
                                        target_frames,
                                        target_frames * 20
                                    );
                                    playback_live = true;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        log::info!("[stage-listener] host stream ended");
                        break;
                    }
                    Err(e) => {
                        log::warn!("[stage-listener] recv error: {e}");
                        break;
                    }
                }
            }
        }
    }

    drop(playback);
    conn.close(0u32.into(), b"listener left");
    log::info!("[stage-listener] playback stopped ({frames} frames decoded)");
}

/// Receive a single forwarded speaker stream from the host (via a QUIC uni-stream),
/// decode Opus to PCM, and push into the speaker mixer for this stream's slot.
/// Deregisters the stream from the mixer on exit.
async fn speaker_stream_recv(
    mut recv: iroh::endpoint::RecvStream,
    stream_id: u32,
    pcm_tx: mpsc::Sender<Vec<f32>>,
    mixer: SpeakerMixerHandle,
    cancel: CancellationToken,
) {
    let mut decoder = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!("[stage-speaker] failed to create stream decoder: {e}");
            mixer.remove_stream(stream_id).await;
            return;
        }
    };

    let mut frames: u32 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = read_audio_frame(&mut recv) => {
                match result {
                    Ok(Some((_, _, _, payload))) => {
                        if let Ok(samples) = decoder.decode(&payload) {
                            if pcm_tx.send(samples).await.is_err() {
                                break;
                            }
                            frames += 1;
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
    }

    mixer.remove_stream(stream_id).await;
    log::debug!("[stage-speaker] stream recv stopped ({frames} frames)");
}

/// Encode mic PCM samples from the AEC output and send to host via a QUIC send stream.
async fn speaker_mic_send(
    mut send: iroh::endpoint::SendStream,
    mut mic_rx: mpsc::Receiver<Vec<f32>>,
    cancel: CancellationToken,
) {
    let mut encoder = match OpusEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            log::error!("[stage-speaker] failed to create mic encoder: {e}");
            return;
        }
    };

    let mut seq: u32 = 0;
    let mut timestamp: u32 = 0;
    let mut frames: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            samples = mic_rx.recv() => {
                let Some(s) = samples else { break };
                for packet in encoder.push_samples(&s) {
                    if write_audio_frame(&mut send, seq, timestamp, TAG_NORMAL, &packet)
                        .await
                        .is_err()
                    {
                        log::warn!("[stage-speaker] mic send failed, host may have disconnected");
                        cancel.cancel();
                        break;
                    }
                    seq = seq.wrapping_add(1);
                    timestamp = timestamp.wrapping_add(SAMPLES_PER_FRAME as u32);
                    frames += 1;
                }
            }
        }
    }

    let _ = send.finish();
    log::info!("[stage-speaker] mic send stopped ({frames} frames)");
}

/// Connect to the host as a speaker (SFU model) with automatic reconnect.
///
/// Each attempt creates its own AEC + capture session (so reconnect gets a fresh mic
/// pipeline). The `SpeakerMixerHandle` (AudioPlayback) persists across reconnects.
/// On each attempt the AEC far-end channel is re-wired via `set_far_end_tx`.
pub(super) async fn start_speaker_pipeline(
    endpoint: Endpoint,
    host_node_id: String,
    speaker_mixer: SpeakerMixerHandle,
    muted: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    const BACKOFF_INITIAL: std::time::Duration = std::time::Duration::from_secs(2);
    const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);
    let mut backoff = BACKOFF_INITIAL;

    let host_id: EndpointId = match host_node_id.parse() {
        Ok(id) => id,
        Err(e) => {
            log::error!("[stage-speaker] invalid host node id: {e}");
            return;
        }
    };

    loop {
        run_speaker_once(
            &endpoint,
            host_id,
            &speaker_mixer,
            muted.clone(),
            cancel.child_token(),
        )
        .await;

        if cancel.is_cancelled() {
            break;
        }

        log::info!(
            "[stage-speaker] disconnected, reconnecting in {}s",
            backoff.as_secs()
        );
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }

    log::info!("[stage-speaker] pipeline stopped");
}

/// One speaker connection attempt: set up AEC + capture, connect to host, run until
/// the connection drops or `session_cancel` fires.
async fn run_speaker_once(
    endpoint: &Endpoint,
    host_id: EndpointId,
    speaker_mixer: &SpeakerMixerHandle,
    muted: Arc<AtomicBool>,
    session_cancel: CancellationToken,
) {
    // Fresh AEC far-end channel for this session.
    let (far_end_tx, mut far_end_rx) = mpsc::channel::<Vec<f32>>(20);
    speaker_mixer.set_far_end_tx(far_end_tx).await;

    // AEC + capture on a dedicated std thread (VoipAec3 is !Send).
    let (cap_tx, mut cap_raw_rx) = mpsc::channel::<Vec<f32>>(32);
    let (fwd_tx, fwd_rx) = mpsc::channel::<Vec<f32>>(32);
    std::thread::spawn(move || {
        let _capture = match AudioCapture::start(cap_tx, None) {
            Ok(c) => c,
            Err(e) => {
                log::error!("[stage-speaker] mic capture failed: {e}");
                return;
            }
        };
        let mut aec = EchoCanceller::new();
        while let Some(s) = cap_raw_rx.blocking_recv() {
            while let Ok(r) = far_end_rx.try_recv() {
                aec.render(&r);
            }
            let cleaned = aec.process_capture(&s);
            if cleaned.is_empty() {
                continue;
            }
            let out = if muted.load(Ordering::Relaxed) {
                vec![0.0f32; cleaned.len()]
            } else {
                cleaned
            };
            if fwd_tx.blocking_send(out).is_err() {
                break;
            }
        }
    });

    let conn = match endpoint
        .connect(EndpointAddr::from(host_id), STAGE_ALPN)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[stage-speaker] failed to connect to host: {e}");
            return;
        }
    };

    let (mut send, _recv) = match conn.open_bi().await {
        Ok(pair) => pair,
        Err(e) => {
            log::warn!("[stage-speaker] failed to open bi-stream: {e}");
            return;
        }
    };

    if send.write_all(&[CONN_TYPE_SPEAKER]).await.is_err() {
        return;
    }

    log::info!("[stage-speaker] connected to host");

    tokio::spawn(speaker_mic_send(send, fwd_rx, session_cancel.child_token()));

    let mut next_stream_id: u32 = 0;
    loop {
        tokio::select! {
            _ = session_cancel.cancelled() => break,
            result = conn.accept_uni() => {
                match result {
                    Ok(uni_recv) => {
                        let stream_id = next_stream_id;
                        next_stream_id = next_stream_id.wrapping_add(1);
                        let (stream_tx, stream_rx) = mpsc::channel::<Vec<f32>>(32);
                        speaker_mixer.add_stream(stream_id, stream_rx).await;
                        tokio::spawn(speaker_stream_recv(
                            uni_recv,
                            stream_id,
                            stream_tx,
                            speaker_mixer.clone(),
                            session_cancel.child_token(),
                        ));
                    }
                    Err(e) => {
                        log::warn!("[stage-speaker] host connection lost: {e}");
                        break;
                    }
                }
            }
        }
    }

    conn.close(0u32.into(), b"speaker left");
}
