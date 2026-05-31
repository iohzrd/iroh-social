use crate::audio::{AudioCapture, EchoCanceller};
use crate::error::AppError;
use crate::stage::command::StageCommand;
use crate::stage::control::ControlPlane;
use crate::stage::fanout::Fanout;
use crate::stage::mixer::spawn_mixer;
use crate::stage::pipeline;
use crate::stage::state::{ActiveStage, Participant, SfuHub};
use crate::stage::topology::TopologyManager;
use crate::stage::{StageActor, control};
use proscenium_types::{
    SignedStageControl, StageControl, StageEvent, StageRole, StageTicket, now_millis, short_id,
    sign_stage_control,
};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tauri::Emitter;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

impl StageActor {
    pub(super) async fn create_stage(&mut self, title: String) -> Result<StageTicket, AppError> {
        if self.active.is_some() {
            return Err(AppError::Other("already in a stage".into()));
        }

        let stage_id = crate::util::generate_id();
        let id = self.identity.read().await;
        let my_pubkey = id.master_pubkey.clone();
        let node_id = id.transport_node_id.clone();
        let signing_key = id.signing_key.clone();
        drop(id);

        // host_pubkey in the ticket must be the *signing* key's public key --
        // that is the key that signs audio stream checkpoints. master_pubkey
        // is a different key and would cause InvalidSignature on every checkpoint.
        let ticket = StageTicket {
            stage_id: stage_id.clone(),
            host_pubkey: signing_key.public().to_string(),
            host_node_id: node_id.clone(),
            title: title.clone(),
        };

        let cancel = CancellationToken::new();
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<SignedStageControl>(64);

        let control_plane = ControlPlane::start(
            &self.gossip,
            &stage_id,
            vec![],
            my_pubkey.clone(),
            StageRole::Host,
            signing_key.clone(),
            Some(node_id.clone()),
            ctrl_tx,
            cancel.child_token(),
        )
        .await?;

        // Broadcast Announce so followers can discover the room via user feed gossip
        // (Phase 3: also broadcast on user feed topic via GossipService)
        let announce = sign_stage_control(
            StageControl::Announce {
                stage_id: stage_id.clone(),
                title: title.clone(),
                host_pubkey: my_pubkey.clone(),
                started_at: now_millis(),
            },
            &signing_key.public().to_string(),
            &signing_key,
            now_millis(),
        );
        let _ = control_plane.broadcast(&announce).await;

        // Host SFU fanout: carries host's own voice to all connected speakers.
        // Created before the mixer so it can be passed in at spawn time.
        let host_sfu_fanout = Arc::new(Fanout::new());
        let sfu_hub = Arc::new(tokio::sync::Mutex::new(SfuHub {
            fanouts: HashMap::new(),
            connections: HashMap::new(),
        }));

        // Start the host audio mixer (owns encoders, auth state, per-speaker PCM buffers)
        let (mixer_handle, fanout) = spawn_mixer(
            signing_key.clone(),
            host_sfu_fanout.clone(),
            cancel.child_token(),
        )?;

        // Add host's own microphone as a speaker input so the host's voice is included in the mix.
        // Also wire a direct PCM channel for mix-minus local playback (host hears others only).
        // AEC: raw mic -> AEC bridge (uses mix-minus as far-end reference) -> mixer.
        let (cap_tx, mut cap_raw_rx) = mpsc::channel::<Vec<f32>>(32);
        let (aec_out_tx, aec_out_rx) = mpsc::channel::<Vec<f32>>(32);
        let (far_end_tx, mut far_end_rx) = mpsc::channel::<Vec<f32>>(20);
        let (host_pcm_tx, host_pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let host_muted_flag = Arc::new(AtomicBool::new(false));
        match mixer_handle
            .add_speaker(my_pubkey.clone(), aec_out_rx)
            .await
        {
            Ok(()) => {
                // Register mix-minus channel: host only hears speakers other than themselves.
                let _ = mixer_handle
                    .set_host_speaker(my_pubkey.clone(), host_pcm_tx)
                    .await;
                let host_pb_cancel = cancel.child_token();
                tokio::spawn(async move {
                    pipeline::run_host_playback(host_pcm_rx, far_end_tx, host_pb_cancel).await;
                });
                // AEC bridge: dedicated std thread owns VoipAec3 (which is !Send).
                // Uses tokio's blocking_recv/try_recv so no async runtime needed.
                let muted_flag = host_muted_flag.clone();
                std::thread::spawn(move || {
                    let mut aec = EchoCanceller::new();
                    while let Some(s) = cap_raw_rx.blocking_recv() {
                        while let Ok(r) = far_end_rx.try_recv() {
                            aec.render(&r);
                        }
                        let cleaned = aec.process_capture(&s);
                        if cleaned.is_empty() {
                            continue;
                        }
                        let out = if muted_flag.load(Ordering::Relaxed) {
                            vec![0.0f32; cleaned.len()]
                        } else {
                            cleaned
                        };
                        if aec_out_tx.blocking_send(out).is_err() {
                            break;
                        }
                    }
                });
                let mic_cancel = cancel.child_token();
                tokio::spawn(async move {
                    let _capture = match AudioCapture::start(cap_tx, None) {
                        Ok(c) => c,
                        Err(e) => {
                            log::warn!("[stage-host] host mic capture failed: {e}");
                            return;
                        }
                    };
                    mic_cancel.cancelled().await;
                });
            }
            Err(e) => {
                log::warn!("[stage-host] failed to register host mic in mixer: {e}");
            }
        }

        let now = now_millis();
        let mut participants = HashMap::new();
        participants.insert(
            my_pubkey.clone(),
            Participant {
                pubkey: my_pubkey.clone(),
                role: StageRole::Host,
                hand_raised: false,
                self_muted: false,
                host_muted: false,
                last_seen_ms: now,
                node_id: Some(node_id.clone()),
            },
        );

        let topology = TopologyManager::new(node_id.clone(), 15);

        self.active = Some(ActiveStage {
            stage_id: stage_id.clone(),
            title,
            host_pubkey: my_pubkey.clone(),
            my_pubkey: my_pubkey.clone(),
            my_role: StageRole::Host,
            self_muted: host_muted_flag,
            started_at: now,
            participants,
            control_plane,
            cancel: cancel.clone(),
            mixer_handle: Some(mixer_handle),
            fanout: Some(fanout),
            topology: Some(topology),
            relay_handle: None,
            ticket: Some(ticket.clone()),
            listener_upstream_id: None,
            listener_pipeline_cancel: None,
            speaker_mixer: None,
            sfu_hub: Some(sfu_hub),
            host_sfu_fanout: Some(host_sfu_fanout),
            host_node_id: node_id.clone(),
            banned_node_ids: std::collections::HashSet::new(),
        });

        // Spawn periodic presence expiry sweep.
        {
            let sweep_tx = self.cmd_rx_forwarder();
            let sweep_cancel = cancel.child_token();
            tokio::spawn(async move {
                let interval = control::PRESENCE_EXPIRY;
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                ticker.tick().await; // skip immediate first tick
                loop {
                    tokio::select! {
                        _ = sweep_cancel.cancelled() => break,
                        _ = ticker.tick() => {
                            if sweep_tx.send(StageCommand::SweepPresence).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        // Forward incoming control messages to the actor via the command channel
        let cmd_tx = self.cmd_rx_forwarder();
        tokio::spawn(async move {
            while let Some(msg) = ctrl_rx.recv().await {
                if cmd_tx
                    .send(StageCommand::ControlReceived(msg))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Announce the stage on our user feed so followers can discover and join.
        // Re-broadcast every 30 s so followers who connect after the initial
        // announcement still see it within a short window.
        {
            let gs = self.gossip_service.clone();
            let ann_stage_id = stage_id.clone();
            let ann_title = self.active.as_ref().unwrap().title.clone();
            let ann_ticket = ticket.clone();
            let ann_pubkey = my_pubkey.clone();
            let ann_cancel = cancel.child_token();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = ann_cancel.cancelled() => break,
                        _ = interval.tick() => {
                            if let Err(e) = gs
                                .broadcast_stage_announcement(
                                    ann_stage_id.clone(),
                                    ann_title.clone(),
                                    ann_ticket.clone(),
                                    ann_pubkey.clone(),
                                    now,
                                )
                                .await
                            {
                                log::warn!("[stage] failed to broadcast stage announcement: {e}");
                            }
                        }
                    }
                }
            });
        }

        log::info!("[stage] created stage {}", short_id(&stage_id));
        self.emit_state_snapshot();
        Ok(ticket)
    }

    pub(super) async fn join_stage(&mut self, ticket: StageTicket) -> Result<(), AppError> {
        if self.active.is_some() {
            return Err(AppError::Other("already in a stage".into()));
        }

        let stage_id = ticket.stage_id.clone();
        let id = self.identity.read().await;
        let my_pubkey = id.master_pubkey.clone();
        let my_node_id = id.transport_node_id.clone();
        let signing_key = id.signing_key.clone();
        drop(id);

        // Resolve the host's transport NodeId so gossip can bootstrap
        let host_node_ids: Vec<iroh::EndpointId> =
            ticket.host_node_id.parse().ok().into_iter().collect();

        let cancel = CancellationToken::new();
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<SignedStageControl>(64);

        let control_plane = ControlPlane::start(
            &self.gossip,
            &stage_id,
            host_node_ids,
            my_pubkey.clone(),
            StageRole::Listener,
            signing_key,
            Some(my_node_id),
            ctrl_tx,
            cancel.child_token(),
        )
        .await?;

        let now = now_millis();
        // Dedicated cancel token for the listener pipeline -- can be replaced on relay reassignment
        let listener_cancel = cancel.child_token();
        self.active = Some(ActiveStage {
            stage_id: stage_id.clone(),
            title: ticket.title.clone(),
            host_pubkey: ticket.host_pubkey.clone(),
            my_pubkey: my_pubkey.clone(),
            my_role: StageRole::Listener,
            self_muted: Arc::new(AtomicBool::new(false)),
            started_at: now,
            participants: HashMap::new(),
            control_plane,
            cancel: cancel.clone(),
            mixer_handle: None,
            fanout: None,
            topology: None,
            relay_handle: None,
            ticket: None,
            listener_upstream_id: Some(ticket.host_node_id.clone()),
            listener_pipeline_cancel: Some(listener_cancel.clone()),
            speaker_mixer: None,
            sfu_hub: None,
            host_sfu_fanout: None,
            host_node_id: ticket.host_node_id.clone(),
            banned_node_ids: std::collections::HashSet::new(),
        });

        // Spawn periodic presence expiry sweep.
        {
            let sweep_tx = self.cmd_rx_forwarder();
            let sweep_cancel = cancel.child_token();
            tokio::spawn(async move {
                let interval = control::PRESENCE_EXPIRY;
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                ticker.tick().await; // skip immediate first tick
                loop {
                    tokio::select! {
                        _ = sweep_cancel.cancelled() => break,
                        _ = ticker.tick() => {
                            if sweep_tx.send(StageCommand::SweepPresence).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        // Forward incoming control messages to the actor
        let cmd_tx = self.cmd_rx_forwarder();
        tokio::spawn(async move {
            while let Some(msg) = ctrl_rx.recv().await {
                if cmd_tx
                    .send(StageCommand::ControlReceived(msg))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Start the listener audio pipeline (connect to host, receive mixed stream)
        let endpoint = self.endpoint.clone();
        let host_node_id = ticket.host_node_id.clone();
        let host_signing_pubkey = ticket.host_pubkey.clone();
        let app_handle_clone = self.app_handle.clone();
        tokio::spawn(async move {
            pipeline::start_listener_pipeline(
                endpoint,
                host_node_id,
                host_signing_pubkey,
                app_handle_clone,
                listener_cancel,
            )
            .await;
        });

        log::info!("[stage] joined stage {}", short_id(&stage_id));
        self.emit_state_snapshot();
        Ok(())
    }

    pub(super) async fn leave_stage(&mut self) -> Result<(), AppError> {
        let stage = self
            .active
            .take()
            .ok_or_else(|| AppError::Other("not in a stage".into()))?;

        let stage_id = stage.stage_id.clone();
        stage.cancel.cancel();
        stage.control_plane.shutdown();

        log::info!("[stage] left stage {}", short_id(&stage_id));
        let _ = self
            .app_handle
            .emit("stage-event", StageEvent::Ended { stage_id });
        Ok(())
    }

    pub(super) async fn end_stage(&mut self) -> Result<(), AppError> {
        {
            let stage = self
                .active
                .as_ref()
                .ok_or_else(|| AppError::Other("not in a stage".into()))?;
            if !matches!(stage.my_role, StageRole::Host) {
                return Err(AppError::Other("only the host can end the stage".into()));
            }
        }

        // Broadcast End before taking the stage so we still have the sender
        self.broadcast_control(|stage_id, pubkey, signing_key| {
            sign_stage_control(
                StageControl::End {
                    stage_id: stage_id.to_string(),
                },
                pubkey,
                signing_key,
                now_millis(),
            )
        })
        .await;

        let stage = self.active.take().unwrap();
        let stage_id = stage.stage_id.clone();
        stage.cancel.cancel();
        stage.control_plane.shutdown();

        // Broadcast ended notification on our user feed so followers know the room closed
        let gs = self.gossip_service.clone();
        let ended_stage_id = stage_id.clone();
        tokio::spawn(async move {
            if let Err(e) = gs.broadcast_stage_ended(ended_stage_id).await {
                log::warn!("[stage] failed to broadcast stage ended: {e}");
            }
        });

        log::info!("[stage] ended stage {}", short_id(&stage_id));
        let _ = self
            .app_handle
            .emit("stage-event", StageEvent::Ended { stage_id });
        Ok(())
    }
}
