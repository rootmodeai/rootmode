//! rootmode desktop — Tauri shell around `rootmode-core`.

pub mod attach;
pub mod commands;
pub mod connected_tools;
pub mod diag;
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
    // Stderr and a file in the app data directory, from the first line. The
    // file is what to ask for when the window shows nothing:
    //   macOS   ~/Library/Application Support/ai.rootmode.desktop/rootmode.log
    //   Linux   ~/.local/share/ai.rootmode.desktop/rootmode.log
    //   Windows %APPDATA%\ai.rootmode.desktop\rootmode.log
    // RUST_LOG still overrides the filter; the `log` macros used elsewhere
    // in this crate are captured too.
    diag::init();

    let stamp = |what: &str| log::info!("[+{}ms] {what}", diag::uptime_ms());

    stamp("building the app");
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // The window's own life, as the OS reports it. A window that was
        // never focused, or never resized past zero, is a different problem
        // from one whose page never loaded.
        .on_window_event(|window, event| {
            use tauri::WindowEvent;
            let label = window.label();
            match event {
                WindowEvent::Focused(f) => log::debug!("window {label}: focused={f}"),
                WindowEvent::Resized(s) => log::debug!("window {label}: resized to {}x{}", s.width, s.height),
                WindowEvent::Moved(p) => log::trace!("window {label}: moved to {},{}", p.x, p.y),
                WindowEvent::ScaleFactorChanged { scale_factor, new_inner_size, .. } => log::info!(
                    "window {label}: scale factor {scale_factor}, inner size {}x{}",
                    new_inner_size.width,
                    new_inner_size.height
                ),
                WindowEvent::ThemeChanged(t) => log::info!("window {label}: theme {t:?}"),
                WindowEvent::CloseRequested { .. } => log::info!("window {label}: close requested"),
                WindowEvent::Destroyed => log::info!("window {label}: destroyed"),
                _ => {}
            }
        })
        // The page's life, as the webview reports it. "started" with no
        // "finished" means the engine gave up on our own HTML.
        .on_page_load(|webview, payload| {
            use tauri::webview::PageLoadEvent;
            match payload.event() {
                PageLoadEvent::Started => log::info!(
                    "[+{}ms] webview {}: page load started: {}",
                    diag::uptime_ms(),
                    webview.label(),
                    payload.url()
                ),
                PageLoadEvent::Finished => log::info!(
                    "[+{}ms] webview {}: page load finished: {}",
                    diag::uptime_ms(),
                    webview.label(),
                    payload.url()
                ),
            }
        })
        .setup(move |app| {
            stamp("setup: started");
            let app_data = app.path().app_data_dir()?;
            let downloads = app
                .path()
                .download_dir()
                .unwrap_or_else(|_| app_data.clone())
                .join("rootmode");
            log::info!("setup: app data {}", app_data.display());
            log::info!("setup: downloads {}", downloads.display());

            let state = Arc::new(AppState::new(app_data, downloads)?);
            stamp("setup: database open, identity loaded");
            crate::pot::boot(state.app_data.clone());
            stamp("setup: wallet ledger restored");

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
                match (window.inner_size(), window.scale_factor(), window.is_visible()) {
                    (Ok(size), Ok(scale), Ok(visible)) => log::info!(
                        "setup: main window exists, {}x{} at scale {scale}, visible={visible}",
                        size.width,
                        size.height
                    ),
                    (size, scale, visible) => log::warn!(
                        "setup: main window exists but will not describe itself: size={size:?} scale={scale:?} visible={visible:?}"
                    ),
                }
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop {
                        paths,
                        position,
                    }) = event
                    {
                        let handle = handle.clone();
                        let paths = paths.clone();
                        // Where it landed, in the page's coordinates, so a
                        // canvas can put the thing under the cursor.
                        let scale = handle
                            .get_webview_window("main")
                            .and_then(|w| w.scale_factor().ok())
                            .unwrap_or(1.0);
                        let at = crate::attach::DropPoint {
                            x: position.x / scale,
                            y: position.y / scale,
                        };
                        tauri::async_runtime::spawn_blocking(move || {
                            let pictures = handle
                                .try_state::<Arc<AppState>>()
                                .map(|s| s.app_data.join(crate::attach::PICTURES_DIR));
                            let outcome =
                                crate::attach::read_all(&paths, pictures.as_deref(), Some(at));
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

            // Keep the spend ledger's settlement links current whether or not
            // the wallet screen is open — the scan is throttled inside.
            {
                let state = state.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
                        match crate::pot::sync_settlements(&state).await {
                            Ok(n) if n > 0 => log::info!("recorded {n} settlement(s) from the chain"),
                            Err(e) => log::debug!("settlement sync: {e}"),
                            _ => {}
                        }
                    }
                });
            }

            // An app left open for days still counts as one that is in use:
            // the update check runs once a day from here as well as on
            // launch. It is the same request the window makes, no more.
            {
                let state = state.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
                        let hello = state.hello();
                        if let Err(e) = crate::update::lookup(hello.as_ref()).await {
                            log::debug!("daily update check: {e}");
                        }
                    }
                });
            }

            // Discovery runs by default and needs no configuration: peers on
            // this network announce themselves and we react to that, rather
            // than polling and making the user wait for the next tick.
            let handle = app.handle().clone();
            stamp("setup: background tasks started");
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

            stamp("setup: complete, handing over to the event loop");
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
            commands::client_log,
            commands::log_path,
            commands::read_picture,
            commands::read_picture_bytes,
            commands::intro_path,
        ])
        .build(tauri::generate_context!());

    let app = match app {
        Ok(app) => app,
        Err(e) => {
            log::error!("could not build the app: {e}");
            panic!("error while building rootmode: {e}");
        }
    };

    stamp("entering the event loop");
    app.run(move |_app, event| {
        use tauri::RunEvent;
        match event {
            // The moment the OS has finished launching us. If the log stops
            // before this line, the window layer never came up at all.
            RunEvent::Ready => stamp("event loop ready: the window is the OS's now"),
            RunEvent::ExitRequested { code, .. } => log::info!("exit requested, code {code:?}"),
            RunEvent::Exit => stamp("exiting"),
            _ => {}
        }
    });
}
