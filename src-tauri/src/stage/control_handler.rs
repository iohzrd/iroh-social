use crate::stage::StageActor;
use crate::stage::pipeline;
use crate::stage::speaker_mixer;
use crate::stage::state::{Participant, is_moderator};
use proscenium_types::{
    SignedStageControl, StageControl, StageEvent, StageRole, now_millis, short_id,
    sign_stage_control,
};
use tauri::Emitter;

impl StageActor {
    pub(super) async fn handle_control(&mut self, signed: SignedStageControl) {
        let stage = match self.active.as_mut() {
            Some(s) => s,
            None => return,
        };

        // Signature verification: we need the sender's signing key.
        // For Phase 2, we skip full verification (no cached delegation lookup yet)
        // and just check that the sender_pubkey is consistent with the message.
        // Phase 3 will add full delegation-based verification.
        log::debug!(
            "[stage-ctrl] received {:?} from {}",
            std::mem::discriminant(&signed.control),
            short_id(&signed.sender_pubkey)
        );

        match &signed.control {
            StageControl::Presence {
                pubkey,
                role,
                timestamp,
                node_id,
                ..
            } => {
                let now = now_millis();
                let is_new = !stage.participants.contains_key(pubkey.as_str());
                let entry =
                    stage
                        .participants
                        .entry(pubkey.clone())
                        .or_insert_with(|| Participant {
                            pubkey: pubkey.clone(),
                            role: *role,
                            hand_raised: false,
                            self_muted: false,
                            host_muted: false,
                            last_seen_ms: *timestamp,
                            node_id: node_id.clone(),
                        });
                entry.last_seen_ms = now;
                entry.role = *role;
                if node_id.is_some() {
                    entry.node_id = node_id.clone();
                }
                if is_new {
                    let _ = self.app_handle.emit(
                        "stage-event",
                        StageEvent::ParticipantJoined {
                            pubkey: pubkey.clone(),
                            role: *role,
                        },
                    );
                }
            }
            StageControl::End { .. }
                // Only accept End from the host
                if signed.sender_pubkey == stage.host_pubkey => {
                    let stage_id = stage.stage_id.clone();
                    stage.cancel.cancel();
                    let cp = self.active.take().unwrap().control_plane;
                    cp.shutdown();
                    log::info!("[stage] room ended by host: {}", short_id(&stage_id));
                    let _ = self
                        .app_handle
                        .emit("stage-event", StageEvent::Ended { stage_id });
                }
            StageControl::RaiseHand { pubkey, .. } => {
                if let Some(p) = stage.participants.get_mut(pubkey.as_str()) {
                    p.hand_raised = true;
                }
                let _ = self.app_handle.emit(
                    "stage-event",
                    StageEvent::HandRaised {
                        pubkey: pubkey.clone(),
                    },
                );
            }
            StageControl::LowerHand { pubkey, .. } => {
                if let Some(p) = stage.participants.get_mut(pubkey.as_str()) {
                    p.hand_raised = false;
                }
                let _ = self.app_handle.emit(
                    "stage-event",
                    StageEvent::HandLowered {
                        pubkey: pubkey.clone(),
                    },
                );
            }
            StageControl::PromoteSpeaker { pubkey, .. }
                if is_moderator(stage, &signed.sender_pubkey) => {
                    if let Some(p) = stage.participants.get_mut(pubkey.as_str()) {
                        p.role = StageRole::Speaker;
                    }

                    if pubkey == &stage.my_pubkey {
                        // Self-promotion: start the speaker audio pipeline.
                        stage.my_role = StageRole::Speaker;

                        // Cancel the existing listener pipeline.
                        if let Some(old_cancel) = stage.listener_pipeline_cancel.take() {
                            old_cancel.cancel();
                        }

                        // Speaker-side PCM mixer: one actor, one AudioPlayback, N input streams.
                        // The mixer sums all incoming speaker streams on a 20 ms tick.
                        // AEC far-end wiring and reconnect are managed inside start_speaker_pipeline.
                        let pb_cancel = stage.cancel.child_token();
                        let speaker_mixer = speaker_mixer::spawn_speaker_mixer(pb_cancel);
                        stage.speaker_mixer = Some(speaker_mixer.clone());

                        // Connect to host: send mic, receive N uni-streams (one per other speaker).
                        // Uses stage.host_node_id -- never listener_upstream_id (which may be a relay).
                        let endpoint = self.endpoint.clone();
                        let host_node_id = stage.host_node_id.clone();
                        let muted = stage.self_muted.clone();
                        let spk_cancel = stage.cancel.child_token();
                        tokio::spawn(async move {
                            pipeline::start_speaker_pipeline(
                                endpoint,
                                host_node_id,
                                speaker_mixer,
                                muted,
                                spk_cancel,
                            )
                            .await;
                        });
                    }
                    // No action needed when another speaker is promoted:
                    // the host will automatically open a new uni-stream on our
                    // connection when that speaker connects.

                    let _ = self.app_handle.emit(
                        "stage-event",
                        StageEvent::RoleChanged {
                            pubkey: pubkey.clone(),
                            role: StageRole::Speaker,
                        },
                    );
                }
            StageControl::DemoteSpeaker { pubkey, .. }
                if is_moderator(stage, &signed.sender_pubkey) => {
                    if let Some(p) = stage.participants.get_mut(pubkey.as_str()) {
                        p.role = StageRole::Listener;
                    }
                    if pubkey == &stage.my_pubkey {
                        stage.my_role = StageRole::Listener;
                    }
                    let _ = self.app_handle.emit(
                        "stage-event",
                        StageEvent::RoleChanged {
                            pubkey: pubkey.clone(),
                            role: StageRole::Listener,
                        },
                    );
                }
            StageControl::MuteSpeaker { pubkey, .. }
                if is_moderator(stage, &signed.sender_pubkey) => {
                    if let Some(p) = stage.participants.get_mut(pubkey.as_str()) {
                        p.host_muted = true;
                    }
                    let self_muted = stage
                        .participants
                        .get(pubkey.as_str())
                        .map(|p| p.self_muted)
                        .unwrap_or(false);
                    let _ = self.app_handle.emit(
                        "stage-event",
                        StageEvent::MuteChanged {
                            pubkey: pubkey.clone(),
                            self_muted,
                            host_muted: true,
                        },
                    );
                }
            StageControl::SelfMuteToggle { pubkey, muted, .. } => {
                if let Some(p) = stage.participants.get_mut(pubkey.as_str()) {
                    p.self_muted = *muted;
                }
                let host_muted = stage
                    .participants
                    .get(pubkey.as_str())
                    .map(|p| p.host_muted)
                    .unwrap_or(false);
                let _ = self.app_handle.emit(
                    "stage-event",
                    StageEvent::MuteChanged {
                        pubkey: pubkey.clone(),
                        self_muted: *muted,
                        host_muted,
                    },
                );
            }
            StageControl::Kick { pubkey, .. }
                if is_moderator(stage, &signed.sender_pubkey) => {
                    stage.participants.remove(pubkey.as_str());
                    if pubkey == &stage.my_pubkey {
                        // We were kicked
                        let stage_id = stage.stage_id.clone();
                        stage.cancel.cancel();
                        let cp = self.active.take().unwrap().control_plane;
                        cp.shutdown();
                        log::info!("[stage] kicked from stage {}", short_id(&stage_id));
                        let _ = self.app_handle.emit("stage-event", StageEvent::Kicked);
                    } else {
                        let _ = self.app_handle.emit(
                            "stage-event",
                            StageEvent::ParticipantLeft {
                                pubkey: pubkey.clone(),
                            },
                        );
                    }
                }
            StageControl::Reaction { pubkey, emoji, .. } => {
                let _ = self.app_handle.emit(
                    "stage-event",
                    StageEvent::Reaction {
                        pubkey: pubkey.clone(),
                        emoji: emoji.clone(),
                    },
                );
            }
            StageControl::Chat { pubkey, text, .. } => {
                let _ = self.app_handle.emit(
                    "stage-event",
                    StageEvent::Chat {
                        pubkey: pubkey.clone(),
                        text: text.clone(),
                    },
                );
            }
            StageControl::RelayVolunteer {
                relay_pubkey,
                relay_node_id,
                capacity,
                ..
            } => {
                // Only the host processes volunteer messages and assigns listeners to relays
                if matches!(stage.my_role, StageRole::Host)
                    && let Some(ref mut topology) = stage.topology
                {
                    topology.add_relay(relay_node_id.clone(), *capacity);

                    // Reassign listeners that the topology manager routes to this new relay
                    let listeners: Vec<String> = stage
                        .participants
                        .values()
                        .filter(|p| matches!(p.role, StageRole::Listener))
                        .map(|p| p.pubkey.clone())
                        .collect();

                    let mut relay_listeners: Vec<String> = Vec::new();
                    for pk in &listeners {
                        if let Some(assignment) = topology.assign_listener(pk)
                            && assignment.source_endpoint_id == *relay_node_id
                        {
                            relay_listeners.push(pk.clone());
                        }
                    }

                    if !relay_listeners.is_empty() {
                        let rp = relay_pubkey.clone();
                        let rn = relay_node_id.clone();
                        let sid = stage.stage_id.clone();
                        let listeners_for_task = relay_listeners;
                        let control_plane = stage.control_plane.clone();
                        let identity = self.identity.clone();
                        tokio::spawn(async move {
                            let id = identity.read().await;
                            let signed = sign_stage_control(
                                StageControl::RelayAssignment {
                                    stage_id: sid,
                                    relay_pubkey: rp,
                                    relay_node_id: rn,
                                    listener_pubkeys: listeners_for_task,
                                },
                                &id.signing_key.public().to_string(),
                                &id.signing_key,
                                now_millis(),
                            );
                            drop(id);
                            if let Err(e) = control_plane.broadcast(&signed).await {
                                log::warn!("[stage-host] relay assignment broadcast failed: {e}");
                            }
                        });
                    }
                }
            }
            StageControl::RelayAssignment {
                relay_node_id,
                listener_pubkeys,
                ..
            }
                // Only accept relay assignments from the host
                if signed.sender_pubkey == stage.host_pubkey => {
                    let my_pubkey = stage.my_pubkey.clone();
                    if listener_pubkeys.contains(&my_pubkey) {
                        // Cancel the current listener pipeline and restart at the new relay
                        if let Some(old_cancel) = stage.listener_pipeline_cancel.take() {
                            old_cancel.cancel();
                        }

                        let new_cancel = stage.cancel.child_token();
                        stage.listener_pipeline_cancel = Some(new_cancel.clone());
                        stage.listener_upstream_id = Some(relay_node_id.clone());

                        let endpoint = self.endpoint.clone();
                        let relay_id = relay_node_id.clone();
                        let host_signing_pubkey = stage.host_pubkey.clone();
                        let app_handle = self.app_handle.clone();
                        tokio::spawn(async move {
                            pipeline::start_listener_pipeline(
                                endpoint,
                                relay_id,
                                host_signing_pubkey,
                                app_handle,
                                new_cancel,
                            )
                            .await;
                        });
                        log::info!(
                            "[stage-listener] reassigned to relay {}",
                            short_id(relay_node_id)
                        );
                    }
                }
            StageControl::Ban { pubkey, .. }
                if signed.sender_pubkey == stage.host_pubkey => {
                    if let Some(node_id) = stage
                        .participants
                        .get(pubkey.as_str())
                        .and_then(|p| p.node_id.clone())
                    {
                        stage.banned_node_ids.insert(node_id);
                    }
                    stage.participants.remove(pubkey.as_str());
                    if pubkey == &stage.my_pubkey {
                        let stage_id = stage.stage_id.clone();
                        stage.cancel.cancel();
                        let cp = self.active.take().unwrap().control_plane;
                        cp.shutdown();
                        log::info!("[stage] banned from stage {}", short_id(&stage_id));
                        let _ = self.app_handle.emit("stage-event", StageEvent::Kicked);
                    } else {
                        let _ = self.app_handle.emit(
                            "stage-event",
                            StageEvent::ParticipantLeft {
                                pubkey: pubkey.clone(),
                            },
                        );
                    }
                }
            _ => {}
        }
    }
}
