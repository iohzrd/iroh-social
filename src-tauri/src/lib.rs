mod audio;
mod call;
mod commands;
mod constants;
mod crypto;
mod device_sync;
mod dm;
mod error;
mod framing;
mod gossip;
mod ingest;
mod peer;
mod preferences;
mod push;
mod setup;
mod stage;
mod state;
mod storage;
mod sync;
mod tasks;
mod util;

use commands::*;
#[cfg(not(mobile))]
use tauri::Manager;

#[cfg(not(mobile))]
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::{
        image::Image,
        menu::{MenuBuilder, MenuItemBuilder},
        tray::TrayIconBuilder,
    };

    let show = MenuItemBuilder::with_id("show", "Show").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;

    // Embed the PNG and decode to RGBA for the tray icon.
    // On Linux, the tray-icon crate re-encodes this as PNG and writes to a temp file
    // for libayatana-appindicator to pick up.
    let png_bytes = include_bytes!("../icons/128x128.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().expect("invalid tray icon png");
    let buf_size = reader.output_buffer_size().expect("invalid png info");
    let mut rgba = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut rgba)
        .expect("failed to decode tray icon");
    rgba.truncate(info.buffer_size());
    let icon = Image::new_owned(rgba, info.width, info.height);

    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Proscenium")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .show_menu_on_left_click(true)
        .build(app)?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut builder = tauri::Builder::default();

    #[cfg(not(mobile))]
    {
        use tauri::Emitter;
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            for arg in &argv {
                if arg.starts_with("proscenium://") {
                    let _ = app.emit("deep-link-received", vec![arg.clone()]);
                    break;
                }
            }
        }));
    }

    #[cfg(mobile)]
    {
        builder = builder
            .plugin(tauri_plugin_barcode_scanner::init())
            .plugin(tauri_plugin_haptics::init());
    }

    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .level_for("iroh", log::LevelFilter::Error)
                .level_for("iroh_gossip", log::LevelFilter::Off)
                .level_for("iroh_quinn_proto", log::LevelFilter::Off)
                .level_for("netlink_packet_route", log::LevelFilter::Off)
                .level_for("tracing", log::LevelFilter::Off)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: None,
                    }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .setup(|app| {
            #[cfg(not(mobile))]
            setup_tray(app)?;
            setup::initialize(app)
        })
        .invoke_handler(tauri::generate_handler![
            get_node_id,
            get_pubkey,
            get_my_profile,
            save_my_profile,
            get_remote_profile,
            create_post,
            delete_post,
            get_feed,
            get_notifications,
            get_unread_notification_count,
            mark_notifications_read,
            get_user_posts,
            sync_posts,
            sync_all_peers,
            get_sync_status,
            like_post,
            unlike_post,
            repost,
            unrepost,
            get_post_counts,
            get_replies,
            get_post,
            follow_user,
            unfollow_user,
            get_follows,
            get_followers,
            get_remote_follows,
            get_remote_followers,
            get_peer_node_ids,
            add_blob,
            fetch_blob,
            add_blob_bytes,
            add_blob_from_path,
            add_blob_from_rgba,
            fetch_blob_bytes,
            refetch_blob_bytes,
            get_node_status,
            send_dm,
            get_conversations,
            get_dm_messages,
            mark_dm_read,
            delete_dm_message,
            get_unread_dm_count,
            send_dm_signal,
            toggle_bookmark,
            is_bookmarked,
            mute_user,
            unmute_user,
            is_muted,
            get_muted_pubkeys,
            block_user,
            unblock_user,
            is_blocked,
            get_blocked_pubkeys,
            get_follow_requests,
            get_pending_follow_request_count,
            approve_follow_request,
            deny_follow_request,
            send_follow_request_to_peer,
            add_server,
            remove_server,
            list_servers,
            refresh_server_info,
            register_with_server,
            unregister_from_server,
            server_get_feed,
            server_get_trending,
            server_search_users,
            server_search_posts,
            server_list_users,
            sync_profile_to_server,
            get_seed_phrase,
            is_seed_phrase_backed_up,
            mark_seed_phrase_backed_up,
            verify_seed_phrase_words,
            rotate_signing_key,
            recover_from_seed_phrase,
            start_device_link,
            cancel_device_link,
            link_with_device,
            get_linked_devices,
            force_device_sync,
            start_call,
            accept_call,
            reject_call,
            hangup_call,
            toggle_mute_call,
            switch_call_input_device,
            switch_call_output_device,
            list_android_audio_devices,
            set_android_audio_device,
            create_stage,
            join_stage,
            leave_stage,
            end_stage,
            get_stage_state,
            stage_promote_speaker,
            stage_demote_speaker,
            stage_toggle_mute,
            stage_raise_hand,
            stage_lower_hand,
            stage_send_reaction,
            stage_send_chat,
            stage_volunteer_relay,
            get_mdns_discovery,
            set_mdns_discovery,
            get_dht_discovery,
            set_dht_discovery,
            get_share_follows,
            set_share_follows,
            get_share_followers,
            set_share_followers,
            list_audio_input_devices,
            list_audio_output_devices,
            get_audio_input_device,
            set_audio_input_device,
            get_audio_output_device,
            set_audio_output_device,
            get_accent_color,
            set_accent_color,
            wipe_all_data,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            #[cfg(not(mobile))]
            if let tauri::RunEvent::WindowEvent {
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } = &event
            {
                // Hide the window instead of closing
                api.prevent_close();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
        });
}
