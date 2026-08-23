//! Stopping cleanly.

/// Resolves on SIGINT (ctrl-c) or, on unix, SIGTERM.
///
/// SIGTERM matters in a container: `docker stop` sends it, and a process that
/// only listens for SIGINT sits there until the runtime loses patience and
/// kills it ten seconds later.
pub async fn signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("cannot listen for SIGTERM: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
