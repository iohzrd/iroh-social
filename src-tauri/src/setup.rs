use crate::call::CallHandler;
use crate::dm::DmHandler;
use crate::error::AppError;
use crate::gossip::GossipService;
use crate::peer::PeerHandler;
use crate::stage::StageHandler;
use crate::state::{AppState, Identity, NetworkStack, SharedIdentity, SyncCommand};
use crate::storage::Storage;
use iroh::{Endpoint, SecretKey, protocol::Router};
use iroh_blobs::{BlobsProtocol, store::fs::FsStore};
use iroh_gossip::Gossip;
use proscenium_types::{
    CALL_ALPN, DM_ALPN, DeviceEntry, LinkedDevicesAnnouncement, PEER_ALPN, STAGE_ALPN,
    derive_dm_key, derive_signing_key, derive_transport_key, now_millis, sign_delegation,
    sign_linked_devices_announcement,
};
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Load raw key bytes from a file, or generate new ones.
fn load_or_create_key_bytes(path: &std::path::Path) -> [u8; 32] {
    if path.exists() {
        let bytes = std::fs::read(path).expect("failed to read key file");
        bytes.try_into().expect("invalid key length")
    } else {
        let mut key_bytes = [0u8; 32];
        getrandom::fill(&mut key_bytes).expect("failed to generate random key");
        std::fs::write(path, key_bytes).expect("failed to write key file");
        key_bytes
    }
}

/// Load the persisted signing key index (returns 0 if not yet saved).
fn load_signing_key_index(data_dir: &std::path::Path) -> u32 {
    let path = data_dir.join("signing_key_index");
    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Load the persisted DM key index (returns 0 if not yet saved).
fn load_dm_key_index(data_dir: &std::path::Path) -> u32 {
    let path = data_dir.join("dm_key_index");
    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Derive all key material from the master key and build an Identity struct.
fn build_identity(data_dir: &std::path::Path) -> Identity {
    let master_secret_key_bytes = load_or_create_key_bytes(&data_dir.join("master_key.key"));
    let transport_key_bytes = derive_transport_key(&master_secret_key_bytes, 0);
    let transport_node_id = SecretKey::from_bytes(&transport_key_bytes)
        .public()
        .to_string();
    let master_secret = SecretKey::from_bytes(&master_secret_key_bytes);
    let signing_key_index = load_signing_key_index(data_dir);
    let signing_secret_key_bytes = derive_signing_key(&master_secret_key_bytes, signing_key_index);
    let signing_key = SecretKey::from_bytes(&signing_secret_key_bytes);
    let dm_key_index = load_dm_key_index(data_dir);
    let dm_secret_key_bytes = derive_dm_key(&master_secret_key_bytes, dm_key_index);
    let (dm_x25519_private, dm_x25519_public) =
        crate::crypto::x25519_keypair_from_raw(&dm_secret_key_bytes);

    let delegation = sign_delegation(
        &master_secret,
        &signing_key.public(),
        signing_key_index,
        &hex::encode(dm_x25519_public),
        dm_key_index,
        now_millis(),
    );

    let ratchet_storage_key = crate::crypto::derive_ratchet_storage_key(&dm_secret_key_bytes);

    Identity {
        master_secret_key_bytes,
        master_pubkey: master_secret.public().to_string(),
        signing_secret_key_bytes,
        signing_key,
        signing_key_index,
        dm_secret_key_bytes,
        dm_pubkey: hex::encode(dm_x25519_public),
        dm_key_index,
        dm_x25519_private,
        dm_x25519_public,
        ratchet_storage_key,
        transport_node_id,
        delegation,
    }
}

/// Build the networking stack: endpoint, gossip, handlers, router.
/// Returns the stack plus channel receivers needed by background tasks.
async fn build_network_stack(
    handle: &tauri::AppHandle,
    identity: SharedIdentity,
    storage: Arc<Storage>,
    blob_store: &FsStore,
    master_secret_key_bytes: &[u8; 32],
) -> Result<
    (
        NetworkStack,
        mpsc::Receiver<SyncCommand>,
        mpsc::UnboundedReceiver<crate::dm::CallSignal>,
    ),
    Box<dyn std::error::Error>,
> {
    let mdns_enabled = crate::preferences::get_mdns_discovery(&storage).await;
    let dht_enabled = crate::preferences::get_dht_discovery(&storage).await;
    log::info!(
        "[setup] mDNS discovery: {}, DHT discovery: {}",
        if mdns_enabled { "enabled" } else { "disabled" },
        if dht_enabled { "enabled" } else { "disabled" },
    );

    let transport_key_bytes = derive_transport_key(master_secret_key_bytes, 0);
    log::info!("[setup] binding iroh endpoint...");
    let mut builder = Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(SecretKey::from_bytes(&transport_key_bytes))
        .alpns(vec![
            iroh_blobs::ALPN.to_vec(),
            iroh_gossip::ALPN.to_vec(),
            PEER_ALPN.to_vec(),
            DM_ALPN.to_vec(),
            CALL_ALPN.to_vec(),
            STAGE_ALPN.to_vec(),
        ]);

    if mdns_enabled {
        builder = builder.address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder());
    }
    if dht_enabled {
        builder = builder.address_lookup(iroh_mainline_address_lookup::DhtAddressLookup::builder());
    }

    let endpoint = builder.bind().await.expect("failed to bind iroh endpoint");

    let blobs_dir = handle
        .path()
        .app_data_dir()
        .expect("failed to resolve app data dir")
        .join("blobs");
    let blobs = BlobsProtocol::new(blob_store, None);
    let gossip = Gossip::builder().spawn(endpoint.clone());
    log::info!("[setup] gossip started");

    let gossip_service = GossipService::new(
        gossip.clone(),
        endpoint.clone(),
        identity.clone(),
        storage.clone(),
        handle.clone(),
    );

    let (call_signal_tx, call_signal_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::dm::CallSignal>();

    let mut dm_handler = DmHandler::new(
        storage.clone(),
        handle.clone(),
        endpoint.clone(),
        identity.clone(),
    );
    dm_handler.set_call_signal_tx(call_signal_tx);

    let call_handler = CallHandler::new(
        storage.clone(),
        endpoint.clone(),
        dm_handler.clone(),
        handle.clone(),
    );

    let stage_handler = StageHandler::new(
        endpoint.clone(),
        gossip_service.gossip_handle(),
        identity.clone(),
        gossip_service.clone(),
        handle.clone(),
    );

    let peer_handler = PeerHandler::new(
        storage.clone(),
        identity.clone(),
        gossip_service.clone(),
        handle.clone(),
        stage_handler.actor_handle(),
    );

    let router = Router::builder(endpoint.clone())
        .accept(iroh_blobs::ALPN, blobs.clone())
        .accept(iroh_gossip::ALPN, gossip)
        .accept(PEER_ALPN, peer_handler.clone())
        .accept(DM_ALPN, dm_handler.clone())
        .accept(CALL_ALPN, call_handler.clone())
        .accept(STAGE_ALPN, stage_handler.clone())
        .spawn();
    log::info!("[setup] router spawned");

    let shutdown = CancellationToken::new();
    let (sync_tx, sync_rx) = mpsc::channel::<SyncCommand>(64);

    let _ = blobs_dir; // used for logging context only in original code

    let net = NetworkStack {
        endpoint,
        gossip: gossip_service,
        dm: dm_handler,
        call: call_handler,
        peer: peer_handler,
        stage: stage_handler,
        blobs,
        sync_tx,
        shutdown,
        _router: router,
    };

    Ok((net, sync_rx, call_signal_rx))
}

/// Start background services on the networking stack.
async fn start_services(
    state: &Arc<AppState>,
    sync_rx: mpsc::Receiver<SyncCommand>,
    mut call_signal_rx: mpsc::UnboundedReceiver<crate::dm::CallSignal>,
) {
    let shutdown = state.shutdown();
    state
        .gossip()
        .start_background(shutdown.child_token())
        .await;
    state.dm().start_background(shutdown.child_token());
    crate::tasks::spawn_all(state.clone(), sync_rx);

    let call = state.call();
    tokio::spawn(async move {
        while let Some(signal) = call_signal_rx.recv().await {
            match signal.payload {
                proscenium_types::DmPayload::CallOffer { call_id, .. } => {
                    call.on_call_offer(&call_id, &signal.peer_pubkey).await;
                }
                proscenium_types::DmPayload::CallAnswer { call_id } => {
                    call.on_call_answered(&call_id, &signal.peer_pubkey).await;
                }
                proscenium_types::DmPayload::CallReject { call_id }
                | proscenium_types::DmPayload::CallHangup { call_id } => {
                    call.on_call_ended(&call_id).await;
                }
                _ => {}
            }
        }
    });
}

/// Broadcast initial gossip state (own feed, profile, device announcement).
async fn broadcast_initial_state(state: &Arc<AppState>) {
    let gossip = state.gossip();

    if let Err(e) = gossip.start_own_feed().await {
        log::error!("[setup] failed to start own feed: {e}");
    } else {
        log::info!("[setup] own gossip feed started");
    }

    {
        let id = state.identity.read().await;
        let master_pubkey = id.master_pubkey.clone();
        drop(id);
        if let Ok(Some(profile)) = state.storage.get_profile(&master_pubkey).await {
            if let Err(e) = gossip.broadcast_profile(&profile).await {
                log::error!("[setup] failed to broadcast profile: {e}");
            } else {
                log::info!("[setup] broadcast profile: {}", profile.display_name);
            }
        }
    }

    {
        let id = state.identity.read().await;
        let now = now_millis();
        if let Err(e) = state
            .storage
            .upsert_linked_device(&id.transport_node_id, "Primary Device", true, true, now)
            .await
        {
            log::error!("[setup] failed to register own device: {e}");
        }

        let mut announcement = LinkedDevicesAnnouncement {
            master_pubkey: id.master_pubkey.clone(),
            delegation: id.delegation.clone(),
            devices: vec![DeviceEntry {
                node_id: id.transport_node_id.clone(),
                device_name: "Primary Device".to_string(),
                is_primary: true,
                added_at: now,
            }],
            version: 1,
            timestamp: now,
            signature: String::new(),
        };
        sign_linked_devices_announcement(&mut announcement, &id.signing_key);
        drop(id);

        if let Err(e) = gossip.broadcast_linked_devices(&announcement).await {
            log::error!("[setup] failed to broadcast device announcement: {e}");
        } else {
            log::info!("[setup] broadcast single-device announcement");
        }
    }
}

/// Rebuild the networking stack after identity recovery.
/// Cancels all background tasks, updates identity from disk, builds fresh
/// networking, and restarts all services.
pub async fn rebuild_for_recovery(state: &Arc<AppState>) -> Result<(), AppError> {
    log::info!("[recovery] starting network rebuild...");

    // 1. Cancel all background tasks.
    state.shutdown().cancel();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 2. Reload key material from disk.
    let data_dir = state.app_handle.path().app_data_dir()?;
    let master_secret_key_bytes = load_or_create_key_bytes(&data_dir.join("master_key.key"));

    // 3. Build new networking stack.
    let (net, sync_rx, call_signal_rx) = build_network_stack(
        &state.app_handle,
        state.identity.clone(),
        state.storage.clone(),
        &state.blob_store,
        &master_secret_key_bytes,
    )
    .await
    .map_err(|e| AppError::Other(e.to_string()))?;

    // 4. Update identity with new key material.
    let new_identity = build_identity(&data_dir);

    log::info!(
        "[recovery] new master pubkey: {}",
        &new_identity.master_pubkey
    );
    log::info!(
        "[recovery] new transport NodeId: {}",
        &new_identity.transport_node_id
    );

    {
        let mut id = state.identity.write().await;
        *id = new_identity;
    }

    // 5. Swap networking stack.
    state.replace_net(net);

    // 6. Start services and broadcast initial state.
    start_services(state, sync_rx, call_signal_rx).await;
    broadcast_initial_state(state).await;

    log::info!("[recovery] network rebuild complete");
    Ok(())
}

pub fn initialize(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        use tauri_plugin_deep_link::DeepLinkExt;
        if let Err(e) = app.deep_link().register_all() {
            log::error!("[setup] failed to register deep link schemes: {e}");
        }
    }

    let handle = app.handle().clone();
    tauri::async_runtime::block_on(setup(handle))
}

async fn setup(handle: tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = handle
        .path()
        .app_data_dir()
        .expect("failed to resolve app data dir");
    std::fs::create_dir_all(&data_dir).expect("failed to create app data dir");
    log::info!("[setup] data dir: {}", data_dir.display());

    let master_secret_key_bytes = load_or_create_key_bytes(&data_dir.join("master_key.key"));

    // Open the database early so we can read preferences before binding the endpoint.
    let storage = Arc::new(
        Storage::open(&data_dir.join("social.db"))
            .await
            .expect("failed to open database"),
    );
    log::info!("[setup] database opened");

    let blobs_dir = data_dir.join("blobs");
    let blob_store = FsStore::load(&blobs_dir)
        .await
        .expect("failed to open blob store");
    log::info!("[setup] blob store opened at {}", blobs_dir.display());

    // Build a placeholder identity (transport_node_id filled in after endpoint bind).
    // We need SharedIdentity before building the network stack because handlers hold it.
    let identity_data = build_identity(&data_dir);

    log::info!("[setup] master pubkey: {}", &identity_data.master_pubkey);
    log::info!(
        "[setup] signing pubkey: {}",
        &identity_data.signing_key.public().to_string()
    );
    log::info!("[setup] DM pubkey: {}", &identity_data.dm_pubkey);
    log::info!(
        "[setup] Transport NodeId: {}",
        &identity_data.transport_node_id
    );

    let identity: SharedIdentity = Arc::new(tokio::sync::RwLock::new(identity_data));

    let (net, sync_rx, call_signal_rx) = build_network_stack(
        &handle,
        identity.clone(),
        storage.clone(),
        &blob_store,
        &master_secret_key_bytes,
    )
    .await?;

    log::info!("[setup] addr (immediate): {:?}", net.endpoint.addr());

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("proscenium/1.0")
        .build()
        .expect("failed to build HTTP client");

    let state = Arc::new(AppState::new(
        handle.clone(),
        identity,
        storage,
        blob_store,
        http_client,
        net,
    ));

    start_services(&state, sync_rx, call_signal_rx).await;
    broadcast_initial_state(&state).await;

    handle.manage(state);
    log::info!("[setup] app state ready");
    Ok(())
}
