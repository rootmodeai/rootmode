//! Ask a worker for a picture, from the command line.
//!
//!     cargo run -p rootmode-worker --example render -- \
//!         ws://sparky1.local:9944 "a lighthouse in a storm" [model]
//!
//! The image-side counterpart of `examples/submit.rs`. It exists because the
//! failures worth catching in this backend — a wrong VAE, a wrong latent — do
//! not throw, they render something that looks wrong, and an operator needs a
//! way to look at the picture without the desktop app in the loop.

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rootmode_core::{
    protocol::ClientMessage, ImageParams, Identity, JobPayload, JobSubmit, WorkerMessage,
};
use uuid::Uuid;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().unwrap_or_else(|| "ws://127.0.0.1:9944".into());
    let prompt = args
        .next()
        .unwrap_or_else(|| "a lighthouse in a storm, oil painting".into());
    let model = args.next();

    let identity = Identity::generate();
    let (mut ws, _) = tokio_tungstenite::connect_async(&endpoint).await?;

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(&ClientMessage::PeerHello(rootmode_core::protocol::PeerHello {
            v: rootmode_core::PROTOCOL_VERSION,
            peer_id: identity.peer_id(),
        }))?,
    ))
    .await?;

    let job_id = Uuid::new_v4();
    let submit = JobSubmit::new(
        job_id,
        identity.peer_id(),
        JobPayload::Image(ImageParams {
            model_hash: None,
            checkpoint_id: model.clone(),
            prompt: prompt.clone(),
            from_image: None,
            change: None,
            mask: None,
        }),
    )
    .signed_by(&identity)?;

    println!("→ {endpoint}");
    println!("  model:  {}", model.as_deref().unwrap_or("(the worker's default)"));
    println!("  prompt: {prompt}");

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(&ClientMessage::JobSubmit(submit))?,
    ))
    .await?;

    while let Some(frame) = ws.next().await {
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame? else {
            continue;
        };
        match serde_json::from_str::<WorkerMessage>(&text) {
            Ok(WorkerMessage::PeerAnnounce(a)) => {
                println!(
                    "  peer:   {} serving {}",
                    a.label.unwrap_or_default(),
                    a.models
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            Ok(WorkerMessage::JobStatus(s)) => {
                println!("  {} {:.0}%", s.status.as_str(), s.progress * 100.0);
                if let Some(e) = s.error {
                    eprintln!("\nfailed: {e}");
                    std::process::exit(1);
                }
                if s.status.is_terminal() {
                    break;
                }
            }
            Ok(WorkerMessage::JobResult(r)) => {
                let Some(encoded) = r.image_path_or_b64 else {
                    eprintln!("no image in the result");
                    std::process::exit(1);
                };
                let bytes = base64::engine::general_purpose::STANDARD.decode(encoded.trim())?;
                let path = std::env::temp_dir().join(format!("rootmode-{}.png", &r.sha256[..12]));
                std::fs::write(&path, &bytes)?;
                println!("\nwrote {} ({} KB)", path.display(), bytes.len() / 1024);
                println!("meta: {}", r.meta);
            }
            _ => {}
        }
    }
    Ok(())
}
