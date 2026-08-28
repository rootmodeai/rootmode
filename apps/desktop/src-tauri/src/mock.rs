//! In-process mock worker.
//!
//! Dev mode: with no real peer reachable, this fulfils jobs locally so the UI,
//! the protocol handling and the result store are all exercised without a GPU.
//! It speaks the same [`WorkerMessage`] stream a real peer would.
//!
//! It is deterministic — the same job payload yields the same bytes and the
//! same sha256 — which makes the content-addressing visible instead of
//! theoretical.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rootmode_core::{
    sha256_hex, ImageParams, JobDelta, JobKind, JobPayload, JobResult, JobStatus, JobStatusUpdate,
    JobSubmit, LlmParams, ModelDescriptor, PeerAnnounce, Price, VideoParams, WorkerMessage,
    PROTOCOL_VERSION, STOPPED,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

use crate::error::Result;
use crate::net::{Probe, Transport};
use crate::store::MOCK_PEER_ID;

/// Wall-clock the mock spends per job, so async UI behaviour is actually
/// testable (progress bars, cancel-free navigation, concurrent jobs).
const STEP_DELAY: Duration = Duration::from_millis(180);
const STEPS: u32 = 6;

pub struct MockTransport;

impl MockTransport {
    pub fn announce() -> PeerAnnounce {
        PeerAnnounce {
            // The mock runs on this machine, wherever that is; claiming a
            // country for it would be inventing one.
            country: None,
            // Nothing to pay: the mock runs in this process.
            payout: None,
            v: PROTOCOL_VERSION,
            peer_id: MOCK_PEER_ID.to_string(),
            label: Some("This computer".to_string()),
            caps: vec!["llm".into(), "image".into(), "video".into()],
            models: vec![
                ModelDescriptor {
                    id: "mock-llm-v0".into(),
                    sha256: Some(sha256_hex(b"mock-llm-v0")),
                    kind: JobKind::Llm,
                    // $20 / million tokens so a 1,000-token mock reply is $0.02
                    // and a 16k ceiling still fits the $0.50 default cap.
                    price: Some(Price::new(20.0)),
                },
                ModelDescriptor {
                    id: "mock-diffusion-v0".into(),
                    sha256: Some(sha256_hex(b"mock-diffusion-v0")),
                    kind: JobKind::Image,
                    price: Some(Price::new(0.02)),
                },
                ModelDescriptor {
                    id: "mock-video-v0".into(),
                    sha256: Some(sha256_hex(b"mock-video-v0")),
                    kind: JobKind::Video,
                    price: None,
                },
            ],
            max_concurrent: 2,
        }
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn probe(&self) -> Result<Probe> {
        Ok(Probe {
            latency_ms: 0,
            announce: Some(MockTransport::announce()),
        })
    }

    async fn run_job(
        &self,
        submit: JobSubmit,
        sink: UnboundedSender<WorkerMessage>,
        stop: Arc<tokio::sync::Notify>,
        _replies: UnboundedReceiver<rootmode_core::ClientMessage>,
    ) -> Result<()> {
        let job_id = submit.job_id;
        let kind = submit.payload.kind();

        send_status(&sink, job_id, JobStatus::Queued, 0.0);
        if sleepable(STEP_DELAY, &stop).await.is_break() {
            return stopped(&sink, job_id);
        }
        send_status(&sink, job_id, JobStatus::Running, 0.05);

        let result = match &submit.payload {
            JobPayload::Llm(p) => {
                let (thinking, result) = llm_result(job_id, p);
                for piece in thinking_pieces(&thinking) {
                    send_delta(&sink, job_id, "", piece);
                    if sleepable(Duration::from_millis(40), &stop).await.is_break() {
                        return stopped(&sink, job_id);
                    }
                }
                let text = result.text.clone().unwrap_or_default();
                for piece in text_pieces(&text) {
                    send_delta(&sink, job_id, piece, "");
                    if sleepable(Duration::from_millis(40), &stop).await.is_break() {
                        return stopped(&sink, job_id);
                    }
                }
                result
            }
            JobPayload::Image(p) => {
                for step in 1..=STEPS {
                    send_status(
                        &sink,
                        job_id,
                        JobStatus::Running,
                        step as f32 / STEPS as f32,
                    );
                    if sleepable(STEP_DELAY, &stop).await.is_break() {
                        return stopped(&sink, job_id);
                    }
                }
                image_result(job_id, p)
            }
            JobPayload::Video(p) => {
                for step in 1..=STEPS {
                    send_status(
                        &sink,
                        job_id,
                        JobStatus::Running,
                        step as f32 / STEPS as f32,
                    );
                    if sleepable(STEP_DELAY, &stop).await.is_break() {
                        return stopped(&sink, job_id);
                    }
                }
                video_result(job_id, p)
            }
        };

        let _ = sink.send(WorkerMessage::JobResult(result));
        send_status(&sink, job_id, JobStatus::Done, 1.0);
        let _ = kind;
        Ok(())
    }
}

/// Sleep, but give up early if the stop button is pressed — this is the
/// mock's whole cooperation with `job.cancel`, since it has no real request
/// to drop.
async fn sleepable(delay: Duration, stop: &tokio::sync::Notify) -> std::ops::ControlFlow<()> {
    tokio::select! {
        biased;
        _ = stop.notified() => std::ops::ControlFlow::Break(()),
        _ = tokio::time::sleep(delay) => std::ops::ControlFlow::Continue(()),
    }
}

fn stopped(sink: &UnboundedSender<WorkerMessage>, job_id: Uuid) -> Result<()> {
    let _ = sink.send(WorkerMessage::JobStatus(JobStatusUpdate {
        v: PROTOCOL_VERSION,
        job_id,
        status: JobStatus::Failed,
        progress: 0.0,
        error: Some(STOPPED.to_string()),
    }));
    Ok(())
}

fn send_status(
    sink: &UnboundedSender<WorkerMessage>,
    job_id: Uuid,
    status: JobStatus,
    progress: f32,
) {
    let _ = sink.send(WorkerMessage::JobStatus(JobStatusUpdate {
        v: PROTOCOL_VERSION,
        job_id,
        status,
        progress,
        error: None,
    }));
}

fn send_delta(sink: &UnboundedSender<WorkerMessage>, job_id: Uuid, text: &str, thinking: &str) {
    if text.is_empty() && thinking.is_empty() {
        return;
    }
    let _ = sink.send(WorkerMessage::JobDelta(JobDelta {
        v: PROTOCOL_VERSION,
        job_id,
        text: text.to_string(),
        thinking: thinking.to_string(),
    }));
}

fn thinking_pieces(thinking: &str) -> Vec<&str> {
    if thinking.is_empty() {
        return Vec::new();
    }
    let mid = thinking.len() / 2;
    let split = thinking
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= mid)
        .unwrap_or(thinking.len());
    vec![&thinking[..split], &thinking[split..]]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect()
}

fn text_pieces(text: &str) -> Vec<&str> {
    if text.len() < 40 {
        return vec![text];
    }
    let mid = text.len() / 2;
    let split = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= mid)
        .unwrap_or(text.len());
    vec![&text[..split], &text[split..]]
}

fn llm_result(job_id: Uuid, p: &LlmParams) -> (String, JobResult) {
    let prompt = p
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.trim())
        .unwrap_or("")
        .to_string();
    let model = p.model_id.clone().unwrap_or_else(|| "mock-llm-v0".into());

    let text = format!(
        "[mock worker] no weights were loaded; this response is generated locally.\n\n\
         prompt ({} chars): {}\n\
         model: {model}\n\
         max_tokens: {}  temperature: {:.2}\n\n\
         Point rootmode at a real peer to get real inference. The message shape, \
         the job id and the sha256 below are identical to what a real worker returns.",
        prompt.chars().count(),
        truncate(&prompt, 240),
        p.max_tokens,
        p.temperature,
    );
    let thinking = format!("The user asked: {}. I'll answer as the mock worker.", truncate(&prompt, 80));

    (
        thinking.clone(),
        JobResult {
            v: PROTOCOL_VERSION,
            job_id,
            kind: JobKind::Llm,
            tool_calls: Vec::new(),
            sha256: sha256_hex(text.as_bytes()),
            text: Some(text),
            image_path_or_b64: None,
            thinking: Some(thinking),
            meta: serde_json::json!({ "model": model, "mock": true, "total_tokens": 1000 }),
        },
    )
}

/// The size the mock renders at. A real worker's size comes from the graph
/// its operator exported; the mock has no graph, so it picks one.
const MOCK_SIZE: (u32, u32) = (768, 768);

fn image_result(job_id: Uuid, p: &ImageParams) -> JobResult {
    // From the prompt, so the same words give the same picture — the mock is
    // for testing, and a test that renders differently each run is no test.
    // A starting picture folds into the seed, so continuing from one gives a
    // different result than starting fresh with the same words, exactly as a
    // real provider would.
    let seed = match &p.from_image {
        None => seed_from(&p.prompt),
        Some(from) => seed_from(&p.prompt) ^ seed_from(from),
    };
    let png = render_png(MOCK_SIZE, seed);
    let model = p
        .checkpoint_id
        .clone()
        .unwrap_or_else(|| "mock-diffusion-v0".into());

    JobResult {
        v: PROTOCOL_VERSION,
        job_id,
        kind: JobKind::Image,
        tool_calls: Vec::new(),
        sha256: sha256_hex(&png),
        text: None,
        image_path_or_b64: Some(base64_encode(&png)),
        thinking: None,
        meta: serde_json::json!({
            "model": model,
            "seed": seed,
            "width": MOCK_SIZE.0,
            "height": MOCK_SIZE.1,
            "mock": true,
        }),
    }
}

const MOCK_VIDEO_SIZE: (u32, u32) = (768, 432);

fn video_result(job_id: Uuid, p: &VideoParams) -> JobResult {
    let seed = match &p.from_image {
        None => seed_from(&p.prompt),
        Some(from) => seed_from(&p.prompt) ^ seed_from(from),
    };
    let png = render_png(MOCK_VIDEO_SIZE, seed);
    let model = p
        .checkpoint_id
        .clone()
        .unwrap_or_else(|| "mock-video-v0".into());

    JobResult {
        v: PROTOCOL_VERSION,
        job_id,
        kind: JobKind::Video,
        tool_calls: Vec::new(),
        sha256: sha256_hex(&png),
        text: None,
        image_path_or_b64: Some(base64_encode(&png)),
        thinking: None,
        meta: serde_json::json!({
            "model": model,
            "seed": seed,
            "width": MOCK_VIDEO_SIZE.0,
            "height": MOCK_VIDEO_SIZE.1,
            "mock": true,
        }),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn seed_from(prompt: &str) -> u64 {
    let digest = sha256_hex(prompt.as_bytes());
    u64::from_str_radix(&digest[..16], 16).unwrap_or(0)
}

/// A deterministic placeholder image: dark field, terminal-green interference
/// pattern keyed by the seed. Not art — a stand-in with the right dimensions,
/// the right file type, and a stable hash.
fn render_png((w, h): (u32, u32), seed: u64) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);

    let mut state = seed | 1;
    let mut next = move || {
        // xorshift64*, so the pattern is reproducible without pulling in rand.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let grain: Vec<u8> = (0..64).map(|_| (next() >> 56) as u8).collect();

    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let wave = ((fx * 24.0).sin() * (fy * 18.0).cos()).abs();
            let g = grain[((x / 8 + y / 8) % 64) as usize] as f32 / 255.0;
            let v = (wave * 0.7 + g * 0.3).clamp(0.0, 1.0);

            rgb.push((8.0 + v * 22.0) as u8); // r — stays near black
            rgb.push((18.0 + v * 190.0) as u8); // g — terminal green
            rgb.push((14.0 + v * 60.0) as u8); // b
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgb).expect("png data");
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootmode_core::ChatMessage;
    use tokio::sync::mpsc;

    fn img(prompt: &str) -> ImageParams {
        ImageParams {
            model_hash: None,
            checkpoint_id: None,
            prompt: prompt.into(),
            from_image: None,
            change: None,
            mask: None,
        }
    }

    #[test]
    fn image_output_is_deterministic_and_is_a_png() {
        let a = image_result(Uuid::nil(), &img("a node you own"));
        let b = image_result(Uuid::nil(), &img("a node you own"));
        assert_eq!(a.sha256, b.sha256);

        let different = image_result(Uuid::nil(), &img("something else"));
        assert_ne!(a.sha256, different.sha256);

        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(a.image_path_or_b64.unwrap())
            .unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            a.sha256,
            sha256_hex(&bytes),
            "sha256 covers the image bytes"
        );
    }

    #[test]
    fn llm_hash_covers_the_text() {
        let (_thinking, r) = llm_result(
            Uuid::nil(),
            &LlmParams {
                model_hash: None,
                model_id: None,
                messages: vec![ChatMessage::new("user", "ping")],
                tools: Vec::new(),
                max_tokens: 16,
                temperature: 0.1,
                reasoning_effort: None,
            },
        );
        assert_eq!(r.sha256, sha256_hex(r.text.as_ref().unwrap().as_bytes()));
    }

    #[tokio::test]
    async fn run_job_streams_status_then_result_then_done() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let submit = JobSubmit::new(
            Uuid::new_v4(),
            "peer",
            JobPayload::Image(img("stream test")),
        );
        MockTransport
            .run_job(
                submit,
                tx,
                std::sync::Arc::new(tokio::sync::Notify::new()),
                crate::net::no_replies(),
            )
            .await
            .unwrap();

        let mut saw_running = false;
        let mut saw_result = false;
        let mut final_status = None;
        while let Ok(m) = rx.try_recv() {
            match m {
                WorkerMessage::JobStatus(s) => {
                    saw_running |= s.status == JobStatus::Running;
                    final_status = Some(s.status);
                }
                WorkerMessage::JobResult(_) => saw_result = true,
                _ => {}
            }
        }
        assert!(saw_running);
        assert!(saw_result);
        assert_eq!(final_status, Some(JobStatus::Done));
    }
}
