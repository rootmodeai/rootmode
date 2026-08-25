//! rootmode desktop — Tauri shell around `rootmode-core`.

pub mod attach;
pub mod commands;
pub mod connected_tools;
pub mod erase;
pub mod error;
pub mod eth_tx;
pub mod gateway;
pub mod identity_store;
pub mod jobs;
pub mod mock;
pub mod net;
pub mod p2p;
pub mod pot;
pub mod results;
pub mod routing;
pub mod state;
pub mod store;
pub mod update;

use std::sync::Arc;

use tauri::{Emitter, Manager};

use crate::state::AppState;

async fn sweep(app: &tauri::AppHandle) {
    match commands::refresh_discovered(app).await {
        Ok(peers) => log::debug!("discovery pass complete, {} peer(s) known", peers.len()),
        Err(e) => log::debug!("discovery: {e}"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Off unless asked for, so a normal run is quiet:
    //   RUST_LOG=rootmode_desktop_lib=debug,rootmode_p2p=debug npm run app
    // The `log` macros used elsewhere in this crate are captured too.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rootmode_desktop_lib=info,rootmode_p2p=info,warn".into()),
        )
        .with_target(false)
        .try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let app_data = app.path().app_data_dir()?;
            let downloads = app
                .path()
                .download_dir()
                .unwrap_or_else(|_| app_data.clone())
                .join("rootmode");

            let state = Arc::new(AppState::new(app_data, downloads)?);
            crate::pot::boot(state.app_data.clone());

            // Jobs cannot outlive the connection that owned them.
            match state.db.fail_orphaned_jobs() {
                Ok(n) if n > 0 => log::info!("marked {n} interrupted job(s) as failed"),
                Err(e) => log::error!("orphan sweep failed: {e}"),
                _ => {}
            }

            let gateway = Arc::new(crate::gateway::Gateway::new());
            // Dropped documents are read here, in the backend, because the
            // path comes from the OS rather than from the window. The drop is
            // the permission; the frontend can never name a file it was not
            // handed.
            {
                let handle = app.handle().clone();
                let window = app
                    .get_webview_window("main")
                    .ok_or("the main window is missing")?;
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop {
                        paths, ..
                    }) = event
                    {
                        let handle = handle.clone();
                        let paths = paths.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            let outcome = crate::attach::read_all(&paths);
                            let _ = handle.emit(commands::EVENT_FILES_DROPPED, outcome);
                        });
                    }
                });
            }

            app.manage(gateway.clone());
            app.manage(state.clone());

            // If the local endpoint was left on, bring it back up before the
            // window appears — an editor configured against it should just
            // work after a restart.
            {
                let state = state.clone();
                let gateway = gateway.clone();
                tauri::async_runtime::spawn(async move {
                    crate::gateway::reconcile(gateway, state).await;
                });
            }

            // Discovery runs by default and needs no configuration: peers on
            // this network announce themselves and we react to that, rather
            // than polling and making the user wait for the next tick.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    if !state.discovery_enabled() {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        continue;
                    }

                    let node = match state.p2p_node().await {
                        Ok(node) => node,
                        Err(e) => {
                            log::warn!("cannot join the network: {e}");
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                            continue;
                        }
                    };

                    let mut events = node.events();
                    sweep(&handle).await;

                    loop {
                        tokio::select! {
                            event = events.recv() => match event {
                                Ok(rootmode_p2p::NodeEvent::PeerDiscovered(peer)) => {
                                    log::info!("peer {peer} appeared; looking at it now");
                                    // A breath so its address is in the routing
                                    // table before we dial.
                                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                                    sweep(&handle).await;
                                }
                                // Lagged just means several arrived at once.
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    sweep(&handle).await;
                                }
                                Err(_) => break, // node stopped; rebuild it
                            },
                            // Backstop for peers beyond this network, and for
                            // ones that came and went while we were busy.
                            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                                sweep(&handle).await;
                            }
                        }

                        if !state.discovery_enabled() {
                            break;
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_identity,
            commands::export_identity_secret,
            commands::import_identity,
            commands::regenerate_identity,
            commands::list_peers,
            commands::add_peer,
            commands::remove_peer,
            commands::probe_peer,
            commands::probe_all_peers,
            commands::discover_peers,
            commands::submit_job,
            commands::stop_job,
            commands::list_jobs,
            commands::get_job,
            commands::get_result,
            commands::list_results,
            commands::read_result_image,
            commands::read_result_bytes,
            commands::reveal_result,
            commands::get_settings,
            commands::set_setting,
            commands::gateway_status,
            commands::rotate_gateway_token,
            commands::list_connected_tools,
            commands::connect_tool,
            commands::disconnect_tool,
            commands::launch_tool,
            commands::dashboard_stats,
            commands::network_status,
            commands::available_models,
            commands::available_providers,
            commands::list_conversations,
            commands::delete_all_conversations,
            commands::conversation_messages,
            commands::new_conversation,
            commands::rename_conversation,
            commands::delete_conversation,
            commands::delete_result,
            commands::add_message,
            commands::pot_status,
            commands::pot_check,
            commands::pot_open_fund,
            commands::pot_deposits,
            commands::token_usage,
            commands::spend_history,
            commands::sync_settlements,
            commands::check_update,
            commands::skip_update,
            commands::open_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running rootmode");
}
