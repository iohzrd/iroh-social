mod initiator;
mod responder;
mod state;
mod termination;

use crate::audio::{AudioCapture, EchoCanceller, android};
use crate::dm::DmHandler;
use crate::storage::Storage;
use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use proscenium_types::{CallEvent, CallState, short_id};
use state::ActiveCall;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct CallHandler {
    pub(crate) storage: Arc<Storage>,
    pub(crate) endpoint: Endpoint,
    pub(crate) dm: DmHandler,
    app_handle: AppHandle,
    pub(crate) active_call: Arc<Mutex<Option<ActiveCall>>>,
}

impl CallHandler {
    pub fn new(
        storage: Arc<Storage>,
        endpoint: Endpoint,
        dm: DmHandler,
        app_handle: AppHandle,
    ) -> Self {
        Self {
            storage,
            endpoint,
            dm,
            app_handle,
            active_call: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn emit_call_event(&self, call_id: &str, peer_pubkey: &str, state: CallState) {
        let _ = self.app_handle.emit(
            "call-state",
            CallEvent {
                call_id: call_id.to_string(),
                peer_pubkey: peer_pubkey.to_string(),
                state,
            },
        );
    }
}

impl std::fmt::Debug for CallHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallHandler").finish_non_exhaustive()
    }
}

impl ProtocolHandler for CallHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let remote = conn.remote_id();
        let remote_str = remote.to_string();
        log::info!(
            "[call] incoming audio connection from {}",
            short_id(&remote_str)
        );

        // Resolve transport NodeId to master pubkey
        let peer_pubkey = self
            .storage
            .get_master_pubkey_for_transport(&remote_str)
            .await
            .unwrap_or_else(|| remote_str.clone());

        // Check we have an active call with this peer
        let (call_id, shared_capture, shared_playback) = {
            let lock = self.active_call.lock().await;
            match &*lock {
                Some(call) if call.peer_pubkey == peer_pubkey => (
                    call.call_id.clone(),
                    call.capture.clone(),
                    call.playback.clone(),
                ),
                _ => {
                    log::warn!(
                        "[call] rejecting unexpected audio connection from {}",
                        short_id(&peer_pubkey)
                    );
                    return Err(AcceptError::from_err(std::io::Error::other(
                        "no active call with this peer",
                    )));
                }
            }
        };

        log::info!(
            "[call] accepting audio stream for call {}",
            short_id(&call_id)
        );

        // The caller opened a bi-stream; accept it for audio data
        let (send, recv) = conn.accept_bi().await?;

        let (session_cancel, muted) = {
            let lock = self.active_call.lock().await;
            let call = lock.as_ref().unwrap();
            (call.cancel.child_token(), call.muted.clone())
        };

        let (input_dev, output_dev) = self.audio_device_prefs().await;

        // On Android, set MODE_IN_COMMUNICATION before opening audio streams
        android::enter_communication_mode();

        // AEC pipeline (same as run_audio_session)
        let (raw_mic_tx, mut raw_mic_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(32);
        let (aec_out_tx, aec_out_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(32);
        let (far_end_tx, mut far_end_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(20);

        let capture = match AudioCapture::start(raw_mic_tx, input_dev.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                log::error!("[call] failed to start audio capture: {e}");
                return Err(AcceptError::from_err(std::io::Error::other(format!(
                    "failed to start audio capture: {e}"
                ))));
            }
        };
        *shared_capture.lock().unwrap() = Some(capture);

        let aec_muted = muted.clone();
        let aec_cancel = session_cancel.clone();
        std::thread::spawn(move || {
            let mut aec = EchoCanceller::new();
            log::info!("[call] AEC thread started (accept)");
            while let Some(raw) = raw_mic_rx.blocking_recv() {
                if aec_cancel.is_cancelled() {
                    break;
                }
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
            log::info!("[call] AEC thread ended (accept)");
        });

        let send_cancel = session_cancel.clone();
        let recv_cancel = session_cancel.clone();

        let send_task = tokio::spawn(Self::send_audio_loop(send, send_cancel, aec_out_rx));
        let recv_task = tokio::spawn(Self::recv_audio_loop(
            recv,
            recv_cancel,
            output_dev,
            shared_playback,
            far_end_tx,
        ));

        tokio::select! {
            _ = session_cancel.cancelled() => {}
            _ = send_task => {}
            _ = recv_task => {}
        }

        // Clean up
        let mut lock = self.active_call.lock().await;
        if lock.as_ref().is_some_and(|c| c.call_id == call_id) {
            let pk = lock.as_ref().unwrap().peer_pubkey.clone();
            *lock = None;
            drop(lock);
            self.emit_call_event(&call_id, &pk, CallState::Ended);
        }

        conn.closed().await;
        Ok(())
    }
}
