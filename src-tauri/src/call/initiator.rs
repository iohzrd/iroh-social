use crate::audio::{
    AudioCapture, AudioPlayback, EchoCanceller, OpusDecoder, OpusEncoder, SAMPLES_PER_FRAME,
    SharedPlayback, TAG_NORMAL, android, read_audio_frame, write_audio_frame,
};
use crate::error::AppError;
use iroh::endpoint::Connection;
use iroh::{EndpointAddr, EndpointId};
use proscenium_types::{CALL_ALPN, CallState, DmPayload, short_id};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio_util::sync::CancellationToken;

use super::state::ActiveCall;

impl super::CallHandler {
    /// Initiate an outgoing call to a peer. Sends a CallOffer via the DM ratchet,
    /// then opens a QUIC connection on CALL_ALPN for audio.
    pub async fn start_call(&self, peer_pubkey: &str) -> Result<String, AppError> {
        let call_id = crate::util::generate_id();
        let cancel = CancellationToken::new();

        // Hold the lock across DM send to prevent rapid-tap races
        {
            let mut lock = self.active_call.lock().await;
            if lock.is_some() {
                return Err(AppError::Other("already in a call".into()));
            }

            log::info!(
                "[call] starting call {} to {}",
                short_id(&call_id),
                short_id(peer_pubkey)
            );

            // Send call offer via E2E encrypted DM channel
            let offer = DmPayload::CallOffer {
                call_id: call_id.clone(),
                video: false,
            };
            self.dm.send_signal(peer_pubkey, offer).await?;

            *lock = Some(ActiveCall {
                call_id: call_id.clone(),
                peer_pubkey: peer_pubkey.to_string(),
                cancel: cancel.clone(),
                muted: Arc::new(AtomicBool::new(false)),
                capture: Arc::new(std::sync::Mutex::new(None)),
                playback: Arc::new(std::sync::Mutex::new(None)),
            });
        }

        self.emit_call_event(&call_id, peer_pubkey, CallState::Ringing);

        // Spawn a timeout: if no answer within 30s, cancel
        let handler = self.clone();
        let cid = call_id.clone();
        let pk = peer_pubkey.to_string();
        let timeout_cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = timeout_cancel.cancelled() => {},
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                    log::info!("[call] timeout waiting for answer on {}", short_id(&cid));
                    handler.emit_call_event(&cid, &pk, CallState::Failed);
                    let mut lock = handler.active_call.lock().await;
                    if lock.as_ref().is_some_and(|c| c.call_id == cid) {
                        *lock = None;
                    }
                }
            }
        });

        Ok(call_id)
    }

    /// Called when we receive a CallAnswer from the peer (via DM handler).
    /// Establishes the audio QUIC connection and starts streaming.
    pub async fn on_call_answered(&self, call_id: &str, peer_pubkey: &str) {
        {
            let mut lock = self.active_call.lock().await;
            match &mut *lock {
                Some(call) if call.call_id == call_id => {
                    // Cancel the ringing timeout, then replace with a fresh token
                    // so the audio session gets a live (uncancelled) token.
                    call.cancel.cancel();
                    call.cancel = CancellationToken::new();
                }
                _ => {
                    log::warn!(
                        "[call] received answer for unknown call {}",
                        short_id(call_id)
                    );
                    return;
                }
            }
        }

        self.emit_call_event(call_id, peer_pubkey, CallState::Active);

        // Resolve peer transport and connect
        let node_ids = self
            .storage
            .get_peer_transport_node_ids(peer_pubkey)
            .await
            .unwrap_or_default();
        let target: Option<EndpointId> = node_ids.iter().find_map(|id| id.parse().ok());

        let Some(target) = target else {
            log::error!("[call] no transport NodeId for {}", short_id(peer_pubkey));
            self.emit_call_event(call_id, peer_pubkey, CallState::Failed);
            *self.active_call.lock().await = None;
            return;
        };

        let conn = match self
            .endpoint
            .connect(EndpointAddr::from(target), CALL_ALPN)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                log::error!("[call] failed to connect for audio: {e}");
                self.emit_call_event(call_id, peer_pubkey, CallState::Failed);
                *self.active_call.lock().await = None;
                return;
            }
        };

        log::info!(
            "[call] audio connection established for {}",
            short_id(call_id)
        );
        let handler = self.clone();
        let cid = call_id.to_string();
        let pk = peer_pubkey.to_string();
        tokio::spawn(async move {
            handler.run_audio_session(conn, &cid, &pk).await;
        });
    }

    /// Run a bidirectional audio session over an established QUIC connection.
    pub(super) async fn run_audio_session(
        &self,
        conn: Connection,
        call_id: &str,
        peer_pubkey: &str,
    ) {
        let (call_cancel, muted, shared_capture, shared_playback) = {
            let lock = self.active_call.lock().await;
            match &*lock {
                Some(c) if c.call_id == call_id => (
                    c.cancel.clone(),
                    c.muted.clone(),
                    c.capture.clone(),
                    c.playback.clone(),
                ),
                _ => return,
            }
        };

        // Open a bi-stream: we send audio on send, receive on recv
        let (send, recv) = match conn.open_bi().await {
            Ok(pair) => pair,
            Err(e) => {
                log::error!("[call] failed to open bi-stream: {e}");
                self.emit_call_event(call_id, peer_pubkey, CallState::Failed);
                *self.active_call.lock().await = None;
                return;
            }
        };

        let session_cancel = call_cancel.child_token();
        let (input_dev, output_dev) = self.audio_device_prefs().await;

        // On Android, set MODE_IN_COMMUNICATION before opening audio streams
        android::enter_communication_mode();

        // AEC pipeline:
        //   mic capture -> raw_mic_tx/rx -> AEC thread -> aec_out_tx/rx -> send loop
        //   recv loop -> far_end_tx/rx -> AEC thread (render)
        let (raw_mic_tx, mut raw_mic_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(32);
        let (aec_out_tx, aec_out_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(32);
        let (far_end_tx, mut far_end_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(20);

        // Start mic capture into the raw channel
        let capture = match AudioCapture::start(raw_mic_tx, input_dev.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                log::error!("[call] failed to start audio capture: {e}");
                self.emit_call_event(call_id, peer_pubkey, CallState::Failed);
                *self.active_call.lock().await = None;
                return;
            }
        };
        *shared_capture.lock().unwrap() = Some(capture);

        // Spawn AEC processing thread (std::thread -- AEC is CPU-bound)
        let aec_muted = muted.clone();
        let aec_cancel = session_cancel.clone();
        std::thread::spawn(move || {
            let mut aec = EchoCanceller::new();
            log::info!("[call] AEC thread started");
            while let Some(raw) = raw_mic_rx.blocking_recv() {
                if aec_cancel.is_cancelled() {
                    break;
                }
                // Drain all pending far-end (playback) samples into AEC render
                while let Ok(far) = far_end_rx.try_recv() {
                    aec.render(&far);
                }
                let cleaned = aec.process_capture(&raw);
                if cleaned.is_empty() {
                    continue;
                }
                let out = if aec_muted.load(std::sync::atomic::Ordering::Relaxed) {
                    vec![0.0f32; cleaned.len()]
                } else {
                    cleaned
                };
                if aec_out_tx.blocking_send(out).is_err() {
                    break;
                }
            }
            log::info!("[call] AEC thread ended");
        });

        // Spawn send task (reads AEC-cleaned samples -> encode -> send)
        let send_cancel = session_cancel.clone();
        let send_task = tokio::spawn(Self::send_audio_loop(send, send_cancel, aec_out_rx));

        // Spawn receive task (receive -> decode -> playback + far-end feed)
        let recv_cancel = session_cancel.clone();
        let recv_task = tokio::spawn(Self::recv_audio_loop(
            recv,
            recv_cancel,
            output_dev,
            shared_playback,
            far_end_tx,
        ));

        // Wait for either task to finish or call to be cancelled
        tokio::select! {
            _ = session_cancel.cancelled() => {
                log::info!("[call] session cancelled for {}", short_id(call_id));
            }
            result = send_task => {
                if let Err(e) = result {
                    log::error!("[call] send task panicked: {e}");
                }
            }
            result = recv_task => {
                if let Err(e) = result {
                    log::error!("[call] recv task panicked: {e}");
                }
            }
        }

        conn.close(0u32.into(), b"call ended");
        log::info!("[call] audio session ended for {}", short_id(call_id));

        // Clean up if we haven't already
        let mut lock = self.active_call.lock().await;
        if lock.as_ref().is_some_and(|c| c.call_id == call_id) {
            let pk = lock.as_ref().unwrap().peer_pubkey.clone();
            *lock = None;
            drop(lock);
            self.emit_call_event(call_id, &pk, CallState::Ended);
        }
    }

    /// Read AEC-cleaned samples, encode to Opus, send over QUIC.
    pub(super) async fn send_audio_loop(
        mut send: iroh::endpoint::SendStream,
        cancel: CancellationToken,
        mut rx: tokio::sync::mpsc::Receiver<Vec<f32>>,
    ) {
        let mut encoder = match OpusEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                log::error!("[call] failed to create Opus encoder: {e}");
                return;
            }
        };

        let mut seq: u32 = 0;
        let mut timestamp: u32 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                samples = rx.recv() => {
                    let Some(samples) = samples else { break };
                    let packets = encoder.push_samples(&samples);
                    for packet in packets {
                        if let Err(e) = write_audio_frame(
                            &mut send, seq, timestamp, TAG_NORMAL, &packet,
                        ).await {
                            log::error!("[call] send error: {e}");
                            return;
                        }
                        seq = seq.wrapping_add(1);
                        timestamp = timestamp.wrapping_add(SAMPLES_PER_FRAME as u32);
                    }
                }
            }
        }

        let _ = send.finish();
        log::info!("[call] send loop ended (sent {seq} frames)");
    }

    /// Receive Opus frames from QUIC, decode, play back, and feed AEC far-end.
    pub(super) async fn recv_audio_loop(
        mut recv: iroh::endpoint::RecvStream,
        cancel: CancellationToken,
        output_device: Option<String>,
        shared_playback: SharedPlayback,
        far_end_tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    ) {
        let mut decoder = match OpusDecoder::new() {
            Ok(d) => d,
            Err(e) => {
                log::error!("[call] failed to create Opus decoder: {e}");
                return;
            }
        };

        let (mut prod, playback) = match AudioPlayback::start(output_device.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("[call] failed to start audio playback: {e}");
                return;
            }
        };

        // Store in shared handle so switch_output_device can reach it
        *shared_playback.lock().unwrap() = Some(playback);

        let mut frames_received: u32 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = read_audio_frame(&mut recv) => {
                    match result {
                        Ok(Some((_seq, _ts, _tag, payload))) => {
                            match decoder.decode(&payload) {
                                Ok(samples) => {
                                    // Feed far-end reference to AEC before playback
                                    let _ = far_end_tx.try_send(samples.clone());
                                    let pushed = prod.push(&samples);
                                    if pushed < samples.len() {
                                        log::debug!("[call] ring buffer full, dropped {} samples", samples.len() - pushed);
                                    }
                                }
                                Err(e) => {
                                    log::warn!("[call] decode error: {e}");
                                }
                            }
                            frames_received += 1;
                        }
                        Ok(None) => {
                            log::info!("[call] remote stream closed");
                            break;
                        }
                        Err(e) => {
                            log::error!("[call] recv error: {e}");
                            break;
                        }
                    }
                }
            }
        }

        // Clear the shared handle
        *shared_playback.lock().unwrap() = None;

        log::info!("[call] recv loop ended ({frames_received} frames received)");
    }
}
