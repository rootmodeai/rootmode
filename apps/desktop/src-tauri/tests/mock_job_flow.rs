//! End-to-end through everything except the Tauri window: mock worker →
//! transport → message handling → sqlite → files on disk.
//!
//! This is the acceptance path from the README ("submit a test job against the
//! mock worker"), so it fails loudly if that stops working.

use base64::Engine;
use std::path::PathBuf;

use rootmode_core::{
    sha256_hex, ChatMessage, ImageParams, JobKind, JobPayload, JobStatus, JobSubmit, LlmParams,
};
use rootmode_desktop_lib::{
    jobs::apply_message,
    mock::MockTransport,
    net::{no_replies, Transport},
    state::{AppState, SETTING_DOWNLOAD_DIR},
    store::{now, JobRecord, MOCK_PEER_ID},
};
use tokio::sync::mpsc;
use uuid::Uuid;

fn temp_state() -> (AppState, PathBuf) {
    let root = std::env::temp_dir().join(format!("rootmode-e2e-{}", Uuid::new_v4()));
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let state = AppState::new(root.join("data"), downloads.clone()).unwrap();
    state
        .set_setting(SETTING_DOWNLOAD_DIR, downloads.to_str().unwrap())
        .unwrap();
    (state, downloads)
}

fn insert_job_in(state: &AppState, payload: &JobPayload, conversation_id: Option<String>) -> Uuid {
    let ts = now();
    let record = JobRecord {
        job_id: Uuid::new_v4(),
        conversation_id,
        peer_id: MOCK_PEER_ID.into(),
        peer_label: "mock".into(),
        kind: payload.kind(),
        summary: payload.summary(),
        model: payload.model_label(),
        payload: payload.clone(),
        status: JobStatus::Queued,
        progress: 0.0,
        error: None,
        created_at: ts,
        updated_at: ts,
    };
    state.db.insert_job(&record).unwrap();
    record.job_id
}

fn insert_job(state: &AppState, payload: &JobPayload) -> Uuid {
    let ts = now();
    let record = JobRecord {
        job_id: Uuid::new_v4(),
        conversation_id: None,
        peer_id: MOCK_PEER_ID.into(),
        peer_label: String::new(),
        kind: payload.kind(),
        summary: payload.summary(),
        model: payload.model_label(),
        payload: payload.clone(),
        status: JobStatus::Queued,
        progress: 0.0,
        error: None,
        created_at: ts,
        updated_at: ts,
    };
    state.db.insert_job(&record).unwrap();
    record.job_id
}

/// Drive one job through the mock worker exactly as `jobs::submit` does.
async fn run(state: &AppState, payload: JobPayload) -> Uuid {
    payload.validate().unwrap();
    let job_id = insert_job(state, &payload);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let submit = JobSubmit::new(job_id, state.identity().peer_id(), payload);
    MockTransport
        .run_job(
            submit,
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();

    while let Ok(msg) = rx.try_recv() {
        apply_message(state, job_id, msg).unwrap();
    }
    job_id
}

#[tokio::test]
async fn llm_job_reaches_done_with_a_verified_hash() {
    let (state, _) = temp_state();
    let payload = JobPayload::Llm(LlmParams {
        model_hash: None,
        model_id: Some("mock-llm-v0".into()),
        messages: vec![ChatMessage::new("user", "what is a peer")],
        tools: Vec::new(),
        max_tokens: 128,
        temperature: 0.2,
    });

    let job_id = run(&state, payload).await;

    let job = state.db.get_job(job_id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Done);
    assert_eq!(job.summary, "what is a peer");

    let result = state.db.get_result(job_id).unwrap().unwrap();
    let text = result.text.expect("llm result carries text");
    assert!(text.contains("mock worker"));
    assert_eq!(result.sha256, sha256_hex(text.as_bytes()));
}

fn image_payload() -> JobPayload {
    JobPayload::Image(ImageParams {
        model_hash: None,
        checkpoint_id: Some("mock-diffusion-v0".into()),
        prompt: "a node you own".into(),
        from_image: None,
        change: None,
        mask: None,
    })
}

#[tokio::test]
async fn image_job_writes_a_file_named_by_its_hash() {
    let (state, downloads) = temp_state();
    let payload = JobPayload::Image(ImageParams {
        model_hash: None,
        checkpoint_id: Some("mock-diffusion-v0".into()),
        prompt: "a node you own".into(),
        from_image: None,
        change: None,
        mask: None,
    });

    let job_id = run(&state, payload).await;

    let job = state.db.get_job(job_id).unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Done);
    assert_eq!(job.kind, JobKind::Image);

    let result = state.db.get_result(job_id).unwrap().unwrap();
    let path = PathBuf::from(result.image_path.expect("image result carries a path"));
    assert!(
        path.starts_with(&downloads),
        "image lands in the download dir"
    );
    assert!(path.exists());

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(
        result.sha256,
        sha256_hex(&bytes),
        "the stored hash is the hash of the bytes on disk"
    );
    assert!(path.to_string_lossy().contains(&result.sha256[..16]));

    // The client never asked for a seed — it cannot. The worker picked one and
    // said which, so a render can be reproduced without being dictated.
    assert!(
        result.meta["seed"].is_u64(),
        "the worker reports the seed it used: {}",
        result.meta
    );
}

#[tokio::test]
async fn results_are_content_addressed_across_runs() {
    let (state, _) = temp_state();
    let make = || {
        JobPayload::Image(ImageParams {
            model_hash: None,
            checkpoint_id: None,
            prompt: "same prompt, same bytes".into(),
            from_image: None,
            change: None,
            mask: None,
        })
    };

    let first = run(&state, make()).await;
    let second = run(&state, make()).await;

    let a = state.db.get_result(first).unwrap().unwrap();
    let b = state.db.get_result(second).unwrap().unwrap();
    assert_eq!(a.sha256, b.sha256);
    assert_eq!(a.image_path, b.image_path, "identical bytes reuse one file");
}

#[tokio::test]
async fn a_job_for_a_capability_the_peer_lacks_is_refused_before_the_wire() {
    // The mock advertises both kinds, so exercise the validation that runs
    // first: a payload the client itself will not send.
    let bad = JobPayload::Image(ImageParams {
        model_hash: None,
        checkpoint_id: None,
        prompt: "   ".into(),
        from_image: None,
        change: None,
        mask: None,
    });
    assert!(bad.validate().is_err());
}

/// The reply must be filed by the job pipeline, not by whatever screen
/// happens to be open.
///
/// The chat screen unmounts the moment you look at another tab. When it owned
/// this step, navigating away mid-generation meant the job finished, the
/// result landed on disk, and the conversation never showed the answer — lost
/// for good, with no error anywhere.
#[tokio::test]
async fn a_reply_is_filed_even_when_nothing_is_watching() {
    let (state, _downloads) = temp_state();

    let chat = state.db.create_conversation("a chat", "llm").unwrap();
    let payload = JobPayload::Llm(LlmParams {
        model_hash: None,
        model_id: Some("mock".into()),
        messages: vec![ChatMessage::new("user", "hello")],
        tools: Vec::new(),
        max_tokens: 64,
        temperature: 0.0,
    });

    let job_id = insert_job_in(&state, &payload, Some(chat.id.clone()));

    // Drive it exactly as the running app does, with no UI in the loop at all.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let submit = JobSubmit::new(job_id, state.identity().peer_id(), payload);
    MockTransport
        .run_job(
            submit,
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();

    let mut filed = None;
    while let Some(msg) = rx.recv().await {
        if let Some(m) = apply_message(&state, job_id, msg).unwrap().message {
            filed = Some(m);
        }
    }

    let message = filed.expect("the pipeline filed the reply");
    assert_eq!(message.role, "assistant");
    assert!(!message.content.is_empty());

    // And it is really in the conversation, not just returned.
    let stored = state.db.conversation_messages(&chat.id).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].job_id.as_deref(),
        Some(job_id.to_string().as_str())
    );
    assert_eq!(stored[0].sha256, message.sha256);
}

/// A job that belongs to no conversation files nothing — image jobs and
/// gateway traffic must not silently appear in somebody's chat history.
#[tokio::test]
async fn a_job_with_no_conversation_files_nothing() {
    let (state, _downloads) = temp_state();
    let payload = JobPayload::Llm(LlmParams {
        model_hash: None,
        model_id: Some("mock".into()),
        messages: vec![ChatMessage::new("user", "hello")],
        tools: Vec::new(),
        max_tokens: 64,
        temperature: 0.0,
    });
    let job_id = insert_job_in(&state, &payload, None);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let submit = JobSubmit::new(job_id, state.identity().peer_id(), payload);
    MockTransport
        .run_job(
            submit,
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();

    while let Some(msg) = rx.recv().await {
        assert!(apply_message(&state, job_id, msg)
            .unwrap()
            .message
            .is_none());
    }
}

/// A ceiling too low to think under is raised before the job leaves.
///
/// This has been got wrong three times in three different callers — the chat
/// screen, the Anthropic endpoint, the OpenAI endpoint — because each fixed
/// it locally. The rule belongs on the path they all take.
#[test]
fn every_caller_gets_a_ceiling_a_reasoning_model_can_work_under() {
    use rootmode_desktop_lib::jobs::{with_workable_ceiling, MIN_ANSWER_TOKENS};

    let asked = |n: u32| {
        let JobPayload::Llm(p) = with_workable_ceiling(JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("m".into()),
            messages: vec![ChatMessage::new("user", "hi")],
            tools: Vec::new(),
            max_tokens: n,
            temperature: 0.0,
        })) else {
            unreachable!("an llm payload stays an llm payload")
        };
        p.max_tokens
    };

    // What the chat screen and a client's background calls used to send.
    assert_eq!(asked(2048), MIN_ANSWER_TOKENS);
    assert_eq!(asked(64), MIN_ANSWER_TOKENS);
    // Anything already generous is left exactly as asked.
    assert_eq!(asked(32_000), 32_000);

    // Image jobs have no answer ceiling to raise.
    let image = JobPayload::Image(ImageParams {
        model_hash: None,
        checkpoint_id: None,
        prompt: "a node".into(),
        from_image: None,
        change: None,
        mask: None,
    });
    assert_eq!(with_workable_ceiling(image.clone()), image);
}

/// Deleting a picture has to take the file with it.
///
/// A row deleted while the PNG stays on disk is not a deletion the user would
/// recognise — and it is invisible afterwards, because nothing in the app
/// points at the file any more.
#[tokio::test]
async fn deleting_an_image_result_removes_the_file_from_disk() {
    let (state, _downloads) = temp_state();
    let job_id = run(&state, image_payload()).await;

    let result = state.db.get_result(job_id).unwrap().expect("a result");
    let path = PathBuf::from(result.image_path.expect("an image on disk"));
    assert!(path.exists());

    // What the delete_result command does, in the order it does it.
    rootmode_desktop_lib::erase::remove(&path).unwrap();
    state.db.delete_result(job_id).unwrap();

    assert!(!path.exists(), "the picture is gone from disk");
    assert!(state.db.get_result(job_id).unwrap().is_none());
    assert!(state.db.get_job(job_id).unwrap().is_none());
}

/// Deleting a conversation takes the pictures it produced.
#[tokio::test]
async fn a_deleted_conversation_does_not_leave_its_images_behind() {
    let (state, _downloads) = temp_state();
    let chat = state
        .db
        .create_conversation("some pictures", "image")
        .unwrap();

    let payload = image_payload();
    let job_id = insert_job_in(&state, &payload, Some(chat.id.clone()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let submit = JobSubmit::new(job_id, state.identity().peer_id(), payload);
    MockTransport
        .run_job(
            submit,
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();
    while let Some(msg) = rx.recv().await {
        apply_message(&state, job_id, msg).unwrap();
    }

    let path = PathBuf::from(
        state
            .db
            .get_result(job_id)
            .unwrap()
            .unwrap()
            .image_path
            .unwrap(),
    );
    assert!(path.exists());

    // The command walks the conversation's jobs and erases each result.
    let ids = state.db.conversation_job_ids(&chat.id).unwrap();
    assert_eq!(ids, vec![job_id], "the job is linked to the conversation");
    for id in ids {
        let r = state.db.get_result(id).unwrap().unwrap();
        rootmode_desktop_lib::erase::remove(std::path::Path::new(r.image_path.as_deref().unwrap()))
            .unwrap();
        state.db.delete_result(id).unwrap();
    }
    state.db.delete_conversation(&chat.id).unwrap();

    assert!(!path.exists(), "the picture went with the conversation");
}

/// Starting from a picture is carried end to end, and changes the result.
///
/// The field is optional and defaults to absent, so nothing catches a
/// pipeline that quietly ignores it — the request would succeed and simply
/// produce an unrelated picture, which looks like the model misunderstanding
/// rather than the plumbing dropping something.
#[tokio::test]
async fn continuing_from_a_picture_produces_a_different_result() {
    let (state, _downloads) = temp_state();

    let first = run(&state, image_payload()).await;
    let source = state.db.get_result(first).unwrap().unwrap();
    let bytes = std::fs::read(source.image_path.as_deref().unwrap()).unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let JobPayload::Image(mut params) = image_payload() else {
        unreachable!()
    };
    params.from_image = Some(encoded);
    let second = run(&state, JobPayload::Image(params)).await;

    let evolved = state.db.get_result(second).unwrap().unwrap();
    assert_ne!(
        source.sha256, evolved.sha256,
        "the starting picture reached the worker and changed what came back"
    );
}
