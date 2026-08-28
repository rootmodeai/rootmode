//! Prove `job.cancel` actually stops a worker, against a real backend.
//!
//! ```sh
//! cargo run -p rootmode-worker --example cancel_check -- ws://sparky1.local:9944 "orcarouter/Qwen3.8-27B-Uncensored-FP8"
//! ```
//!
//! Submits a job with a long answer ceiling, waits until real tokens are
//! streaming, sends `job.cancel`, and checks that the worker reports the job
//! stopped rather than finished. It does not check GPU utilization itself —
//! that is watched by hand on the box (`nvidia-smi`) while this runs, which is
//! the only way to see the thing that actually matters: whether the upstream
//! server stopped generating, not just whether this socket stopped listening.

use futures_util::{SinkExt, StreamExt};
use rootmode_core::{
    protocol::{ClientMessage, JobCancel, PeerHello},
    ChatMessage, Identity, JobPayload, JobStatus, JobSubmit, LlmParams, WorkerMessage,
    PROTOCOL_VERSION,
};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().unwrap_or_else(|| "ws://127.0.0.1:9944".into());
    let model = args.next();

    let identity = Identity::generate();
    let (mut ws, _) = tokio_tungstenite::connect_async(&endpoint).await?;

    ws.send(Message::Text(serde_json::to_string(&ClientMessage::PeerHello(
        PeerHello { v: PROTOCOL_VERSION, peer_id: identity.peer_id() },
    ))?))
    .await?;

    let job_id = Uuid::new_v4();
    let submit = JobSubmit::new(
        job_id,
        identity.peer_id(),
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: model,
            messages: vec![ChatMessage::new(
                "user",
                "Write a very long, detailed essay — at least 2000 words — about the history \
                 of the printing press, its social effects, and its economic effects. Do not \
                 stop until you have written the whole thing.",
            )],
            tools: Vec::new(),
            max_tokens: 8000,
            temperature: 0.7,
            reasoning_effort: None,
        }),
    )
    .signed_by(&identity)?;

    println!("job      {job_id}");
    ws.send(Message::Text(serde_json::to_string(&ClientMessage::JobSubmit(submit))?))
        .await?;

    let mut tokens_seen = 0usize;
    let mut cancelled = false;
    let started = std::time::Instant::now();

    while let Some(frame) = ws.next().await {
        let Message::Text(text) = frame? else { continue };
        match WorkerMessage::parse(&text) {
            Ok(WorkerMessage::JobDelta(d)) => {
                tokens_seen += d.text.len();
                // Once real generation is under way, ask it to stop — this is
                // the moment a person would have clicked Stop.
                if !cancelled && tokens_seen > 20 {
                    cancelled = true;
                    println!(
                        "cancel   sending job.cancel after {} chars in {:.1}s",
                        tokens_seen,
                        started.elapsed().as_secs_f32()
                    );
                    ws.send(Message::Text(serde_json::to_string(&ClientMessage::JobCancel(
                        JobCancel { job_id },
                    ))?))
                    .await?;
                }
            }
            Ok(WorkerMessage::JobStatus(s)) => {
                if let Some(error) = &s.error {
                    println!(
                        "status   {} — {error} (after {:.1}s, saw {} chars)",
                        s.status.as_str(),
                        started.elapsed().as_secs_f32(),
                        tokens_seen
                    );
                }
                if s.status == JobStatus::Done || s.status == JobStatus::Failed {
                    if !cancelled {
                        println!("FAIL     finished before a cancel was ever sent — raise max_tokens");
                    } else if s.status == JobStatus::Done {
                        println!(
                            "FAIL     worker reports Done — job.cancel did not stop generation"
                        );
                    } else if s.error.as_deref() == Some(rootmode_core::STOPPED) {
                        println!(
                            "OK       worker stopped {:.1}s after cancel, generation did not run to completion",
                            started.elapsed().as_secs_f32()
                        );
                    } else {
                        println!("FAIL     failed for a different reason: {:?}", s.error);
                    }
                    break;
                }
            }
            Ok(WorkerMessage::JobResult(_)) => {
                println!("FAIL     a full result arrived — the job was not actually stopped");
            }
            _ => {}
        }
    }

    Ok(())
}
