//! Submit one job to a rootmode worker and print what comes back.
//!
//! For proving a node works without opening the desktop client:
//!
//! ```sh
//! cargo run -p rootmode-worker --example submit -- ws://127.0.0.1:9944 "what is a peer?"
//! cargo run -p rootmode-worker --example submit -- --image ws://host:9944 "a quiet room at dusk"
//! cargo run -p rootmode-worker --example submit -- --image --from before.png ws://host:9944 "…, with a cat"
//! ```
//!
//! With `--image` the result is written next to you as a `.png`, because the
//! point of proving an image node works is looking at what it drew.
//!
//! It speaks the documented wire format directly, so it is also the smallest
//! readable example of a rootmode client.

use futures_util::{SinkExt, StreamExt};
use rootmode_core::{
    protocol::{ClientMessage, PeerHello},
    ChatMessage, Identity, ImageParams, JobPayload, JobStatus, JobSubmit, LlmParams, WorkerMessage,
    PROTOCOL_VERSION,
};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let image = args.first().is_some_and(|a| a == "--image");
    if image {
        args.remove(0);
    }
    // `--from picture.png` — evolve an existing picture rather than start fresh.
    let mut from: Option<String> = None;
    if args.first().is_some_and(|a| a == "--from") {
        args.remove(0);
        from = Some(args.remove(0));
    }
    // `--change 0.6` — how much of the starting picture to let go of.
    let mut change: Option<f32> = None;
    if args.first().is_some_and(|a| a == "--change") {
        args.remove(0);
        change = args.remove(0).parse().ok();
    }
    // `--mask mask.png` — white where it may repaint, black where it must not.
    let mut mask: Option<String> = None;
    if args.first().is_some_and(|a| a == "--mask") {
        args.remove(0);
        mask = Some(args.remove(0));
    }
    let mut args = args.into_iter();
    let endpoint = args.next().unwrap_or_else(|| "ws://127.0.0.1:9944".into());
    let prompt = args.next().unwrap_or_else(|| "what is a peer?".into());
    let model = args.next();

    // Ephemeral identity: this is a probe, not a node.
    let identity = Identity::generate();
    println!("client   {}", identity.peer_id());
    println!("endpoint {endpoint}");

    let (mut ws, _) = tokio_tungstenite::connect_async(&endpoint).await?;

    ws.send(Message::Text(serde_json::to_string(
        &ClientMessage::PeerHello(PeerHello {
            v: PROTOCOL_VERSION,
            peer_id: identity.peer_id(),
        }),
    )?))
    .await?;

    // An image job is a model and words. How it is rendered belongs to
    // whoever set the worker up.
    let payload = if image {
        JobPayload::Image(ImageParams {
            model_hash: None,
            checkpoint_id: model,
            prompt,
            // `--from <file.png>` starts from a picture instead of noise.
            from_image: from.map(|path| {
                use base64::Engine;
                let bytes =
                    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
                base64::engine::general_purpose::STANDARD.encode(bytes)
            }),
            change,
            // `--mask picture.png` — repaint only where the mask is white.
            mask: mask.map(|path| {
                use base64::Engine;
                let bytes =
                    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
                base64::engine::general_purpose::STANDARD.encode(bytes)
            }),
        })
    } else {
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: model,
            messages: vec![ChatMessage::new("user", prompt)],
            tools: Vec::new(),
            max_tokens: 8192,
            temperature: 0.7,
        })
    };

    let submit =
        JobSubmit::new(Uuid::new_v4(), identity.peer_id(), payload).signed_by(&identity)?;

    println!("job      {}", submit.job_id);
    ws.send(Message::Text(rootmode_core::canonical::wire_json(
        &ClientMessage::JobSubmit(submit),
    )?))
    .await?;

    while let Some(frame) = ws.next().await {
        let Message::Text(text) = frame? else {
            continue;
        };
        match WorkerMessage::parse(&text) {
            Ok(WorkerMessage::PeerAnnounce(a)) => {
                println!("worker   {}", a.peer_id);
                println!("caps     [{}]", a.caps.join(", "));
                println!(
                    "models   {}",
                    if a.models.is_empty() {
                        "(none advertised)".to_string()
                    } else {
                        a.models
                            .iter()
                            .map(|m| m.id.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
            }
            Ok(WorkerMessage::JobStatus(s)) => {
                match &s.error {
                    Some(error) => println!("status   {} — {error}", s.status.as_str()),
                    None => println!("status   {} {:.0}%", s.status.as_str(), s.progress * 100.0),
                }
                if s.status == JobStatus::Failed {
                    std::process::exit(1);
                }
                if s.status == JobStatus::Done {
                    break;
                }
            }
            Ok(WorkerMessage::JobResult(r)) => {
                println!("sha256   {}", r.sha256);
                println!("meta     {}", r.meta);
                println!("---");
                match (&r.text, &r.image_path_or_b64) {
                    (Some(text), _) => println!("{text}"),
                    (_, Some(encoded)) => {
                        use base64::Engine;
                        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
                        // Verify before writing: a result nobody checked is a
                        // result nobody should keep.
                        let actual = rootmode_core::sha256_hex(&bytes);
                        if actual != r.sha256 {
                            eprintln!(
                                "hash mismatch: worker said {}, bytes are {actual}",
                                r.sha256
                            );
                            std::process::exit(1);
                        }
                        let out = format!("{}.png", &actual[..16]);
                        std::fs::write(&out, &bytes)?;
                        println!("wrote {out} ({} bytes, hash verified)", bytes.len());
                    }
                    _ => println!("<empty result>"),
                }
                println!("---");
            }
            Ok(WorkerMessage::JobDelta(_)) => {}
            Ok(WorkerMessage::JobInvoice(i)) => {
                println!("invoice  {} micros", i.amount);
            }
            Ok(WorkerMessage::Unknown) => {}
            Err(e) => eprintln!("ignoring frame: {e}"),
        }
    }

    Ok(())
}
