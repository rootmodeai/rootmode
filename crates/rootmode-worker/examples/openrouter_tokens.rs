//! Live check: does our OpenAI tokenizer match what OpenRouter actually bills?
//!
//! Hits OpenRouter the same way a seed worker does (streaming, `usage.include`,
//! `stream_options.include_usage`), counts the same prompt and answer with
//! [`rootmode_core::TokenUsage`], then asks OpenRouter for the generation
//! record — that last number is what they charge. Prints the three side by
//! side so a mismatch is visible, and exits non-zero if we would under-bill.
//!
//! ```sh
//! export ROOTMODE_OPENROUTER_KEY=sk-or-...
//! cargo run -p rootmode-worker --example openrouter_tokens
//! cargo run -p rootmode-worker --example openrouter_tokens -- openai/gpt-4o-mini "say hi in five words"
//! cargo run -p rootmode-worker --example openrouter_tokens -- meta-llama/llama-3.3-70b-instruct "ping"
//! ```
//!
//! `openai/gpt-4o-mini` should line up: same tokenizer. A Llama or Qwen model
//! will often not — that is the case this exists to catch.

use std::time::Duration;

use futures_util::StreamExt;
use rootmode_core::{ChatMessage, LlmParams, TokenUsage};
use serde::Deserialize;
use serde_json::Value;

const BASE: &str = "https://openrouter.ai/api/v1";
const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";
const DEFAULT_PROMPT: &str = "Reply with exactly: ok";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("ROOTMODE_OPENROUTER_KEY")
        .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
        .map_err(|_| {
            "set ROOTMODE_OPENROUTER_KEY (or OPENROUTER_API_KEY) to an OpenRouter key"
        })?;

    let mut args = std::env::args().skip(1);
    let model = args.next().unwrap_or_else(|| DEFAULT_MODEL.into());
    let prompt = args.next().unwrap_or_else(|| DEFAULT_PROMPT.into());

    let params = LlmParams {
        model_hash: None,
        model_id: Some(model.clone()),
        messages: vec![ChatMessage::new("user", prompt.clone())],
        tools: Vec::new(),
        max_tokens: 64,
        temperature: 0.0,
    };

    println!("model    {model}");
    println!("prompt   {prompt}");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": params.max_tokens,
        "temperature": params.temperature,
        "stream": true,
        "stream_options": { "include_usage": true },
        "usage": { "include": true },
    });

    let resp = http
        .post(format!("{BASE}/chat/completions"))
        .bearer_auth(&key)
        .header("HTTP-Referer", "https://rootmode.ai")
        .header("X-Title", "rootmode-token-check")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("OpenRouter {status}: {}", text.chars().take(400).collect::<String>()).into());
    }

    let header_id = resp
        .headers()
        .get("x-generation-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut generation_id = String::new();
    let mut stream_usage: Option<StreamUsage> = None;

    while let Some(next) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&next?));
        while let Some(idx) = buffer.find('\n') {
            let line = buffer[..idx].trim().to_string();
            buffer.drain(..=idx);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(chunk) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                if generation_id.is_empty() && id.starts_with("gen-") {
                    generation_id = id.to_string();
                }
            }
            if let Some(usage) = chunk.get("usage").cloned() {
                if let Ok(parsed) = serde_json::from_value::<StreamUsage>(usage) {
                    stream_usage = Some(parsed);
                }
            }
            for choice in chunk.get("choices").and_then(Value::as_array).into_iter().flatten()
            {
                let Some(delta) = choice.get("delta") else {
                    continue;
                };
                if let Some(c) = delta.get("content").and_then(Value::as_str) {
                    text.push_str(c);
                }
                if let Some(c) = delta.get("reasoning_content").and_then(Value::as_str) {
                    thinking.push_str(c);
                }
            }
        }
    }

    if generation_id.is_empty() {
        generation_id = header_id.clone();
    }
    if generation_id.is_empty() {
        return Err("OpenRouter returned no generation id".into());
    }

    println!("id       {generation_id}");
    if !thinking.is_empty() {
        println!(
            "think    {}",
            thinking.chars().take(80).collect::<String>()
        );
    }
    println!(
        "answer   {}",
        if text.is_empty() {
            "(empty)".into()
        } else {
            text.chars().take(120).collect::<String>()
        }
    );
    println!();

    let tokenizer = TokenUsage::measure(
        &params,
        Some(&text),
        if thinking.is_empty() {
            None
        } else {
            Some(thinking.as_str())
        },
        &[],
    );
    let stream = stream_usage.as_ref().map(TokenUsage::from);
    let billed_us = tokenizer.reconcile(stream);

    let generation = fetch_generation(&http, &key, &generation_id).await;

    println!(
        "{:<14} {:>10} {:>10} {:>10} {:>10}",
        "", "tokenizer", "stream", "billed", "we charge"
    );
    row(
        "prompt",
        Some(tokenizer.prompt),
        stream.map(|s| s.prompt),
        generation.as_ref().and_then(Generation::prompt),
        billed_us.prompt,
    );
    row(
        "completion",
        Some(tokenizer.completion),
        stream.map(|s| s.completion),
        generation.as_ref().and_then(Generation::completion),
        billed_us.completion,
    );
    row(
        "cached",
        Some(tokenizer.cached),
        stream.map(|s| s.cached),
        generation.as_ref().and_then(|g| g.native_tokens_cached),
        billed_us.cached,
    );
    row(
        "reasoning",
        Some(tokenizer.reasoning),
        stream.map(|s| s.reasoning),
        generation.as_ref().and_then(|g| g.native_tokens_reasoning),
        billed_us.reasoning,
    );
    println!();
    println!(
        "billed  = GET /api/v1/generation native tokens (what OpenRouter charges)"
    );
    println!("stream  = usage object on the completion stream");
    println!("we charge = tokenizer, raised to the stream when that is higher");
    if let Some(cost) = generation
        .as_ref()
        .and_then(|g| g.usage)
        .or(stream_usage.as_ref().and_then(|u| u.cost))
    {
        println!("cost    = ${cost:.8}");
        if let Some(provider) = generation.as_ref().and_then(|g| g.provider_name.as_ref()) {
            println!("via     {provider}");
        }
    } else {
        println!("cost    = (not returned)");
    }
    if generation.is_none() {
        println!(
            "note    generation record 404'd — comparing against the stream usage, \
             which is what a seed worker would see"
        );
    }

    let billed_prompt = generation
        .as_ref()
        .and_then(Generation::prompt)
        .or(stream.map(|s| s.prompt));
    let billed_completion = generation
        .as_ref()
        .and_then(Generation::completion)
        .or(stream.map(|s| s.completion));

    let mut lost = false;
    for (name, ours, theirs) in [
        ("prompt", billed_us.prompt, billed_prompt),
        ("completion", billed_us.completion, billed_completion),
    ] {
        if let Some(theirs) = theirs {
            if ours < theirs {
                println!("UNDER  {name}: we charge {ours}, they billed {theirs}");
                lost = true;
            }
        }
    }
    if billed_prompt.is_none() && billed_completion.is_none() {
        println!("no usage from stream or generation — cannot compare");
        std::process::exit(2);
    }
    if lost {
        println!("\nwe would lose money on this request");
        std::process::exit(1);
    }
    println!("\naligned or we charge more — we would not lose money");
    Ok(())
}

fn row(name: &str, tokenizer: Option<u64>, stream: Option<u64>, billed: Option<u64>, charge: u64) {
    println!(
        "{:<14} {:>10} {:>10} {:>10} {:>10}",
        name,
        n(tokenizer),
        n(stream),
        n(billed),
        charge
    );
}

fn n(v: Option<u64>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "—".into())
}

#[derive(Deserialize, Default)]
struct StreamUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionDetails>,
    #[serde(default)]
    cost: Option<f64>,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl From<&StreamUsage> for TokenUsage {
    fn from(u: &StreamUsage) -> Self {
        TokenUsage {
            prompt: u.prompt_tokens,
            completion: u.completion_tokens,
            cached: u
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0)
                .min(u.prompt_tokens),
            reasoning: u
                .completion_tokens_details
                .as_ref()
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0),
        }
    }
}

#[derive(Deserialize, Default)]
struct Generation {
    #[serde(default)]
    native_tokens_prompt: Option<u64>,
    #[serde(default)]
    native_tokens_completion: Option<u64>,
    #[serde(default)]
    native_tokens_cached: Option<u64>,
    #[serde(default)]
    native_tokens_reasoning: Option<u64>,
    #[serde(default)]
    tokens_prompt: Option<u64>,
    #[serde(default)]
    tokens_completion: Option<u64>,
    #[serde(default)]
    usage: Option<f64>,
    #[serde(default)]
    provider_name: Option<String>,
}

impl Generation {
    fn prompt(&self) -> Option<u64> {
        self.native_tokens_prompt.or(self.tokens_prompt)
    }

    fn completion(&self) -> Option<u64> {
        self.native_tokens_completion.or(self.tokens_completion)
    }
}

#[derive(Deserialize)]
struct GenerationWrap {
    data: Generation,
}

/// The generation record lags the stream by a moment. 404 here is "not
/// settled yet", not "wrong id".
async fn fetch_generation(
    http: &reqwest::Client,
    key: &str,
    id: &str,
) -> Option<Generation> {
    let url = format!("{BASE}/generation?id={id}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let resp = http.get(&url).bearer_auth(key).send().await.ok()?;
        if resp.status().is_success() {
            return resp.json::<GenerationWrap>().await.ok().map(|w| w.data);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}
