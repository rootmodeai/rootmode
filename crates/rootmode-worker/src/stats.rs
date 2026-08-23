//! Reporting what this node served, to a stats collector.
//!
//! Off unless an operator sets `[stats] url`. What it sends is what this
//! worker did — job counts and token totals — and never what anyone asked
//! for: no prompts, no results, no client peer ids. The collector learns that
//! a node answered 4,000 requests today, not who wanted them.
//!
//! Reports are signed with the node's own key, so a collector can tell a real
//! worker's numbers from an invented node's, and are sent from the worker
//! itself, which is what lets the collector see the address and place the node
//! on a map without anybody shipping an address list around.

use std::sync::Mutex;
use std::time::Duration;

use rootmode_core::{canonical::canonical_bytes, Identity, JobKind, JobResult, Price, TokenUsage};
use serde::{Deserialize, Serialize};

/// What one node did over one window. Cleared each time it is sent, so a
/// dropped report costs that window rather than double-counting the next.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Counters {
    pub requests: u64,
    pub images: u64,
    /// Tokens the model read. Counted with the OpenAI tokenizer, then raised
    /// to the provider's figure when that is higher, so an under-report
    /// cannot shrink the bill.
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Subset of `tokens_in` served from a provider cache. Only a provider
    /// knows this; zero means none were reported, not "none were used".
    /// Omitted when zero so existing signed reports stay valid.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tokens_cached: u64,
    /// What this node charged over the window, in `currency`.
    pub revenue: f64,
    /// Ran and did not produce a result: a backend error, a timeout, an
    /// inference server that fell over.
    pub failures: u64,
    /// Turned away before any work started — unsigned when signatures are
    /// required, not on the allowlist, out of bounds, screened out, or asking
    /// for a model this node does not serve.
    ///
    /// Counted apart from `failures` because they mean opposite things: one is
    /// this node failing, the other is this node working exactly as
    /// configured.
    pub rejected: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Counters {
    fn is_empty(&self) -> bool {
        *self == Counters::default()
    }
}

/// The report body. `sig` covers the canonical JSON of everything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub v: u32,
    pub peer_id: String,
    pub label: String,
    /// The operator's own declaration, if they made one. The collector
    /// geolocates the connection as well, and prefers this when both exist —
    /// an operator knows where their machine is better than a database does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    pub caps: Vec<String>,
    pub models: Vec<String>,
    /// Seconds covered by these counters, so a collector can tell a busy hour
    /// from a busy minute.
    pub window_secs: u64,
    #[serde(flatten)]
    pub counters: Counters,
    pub currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl Report {
    pub fn signed_by(mut self, identity: &Identity) -> Result<Self, String> {
        self.sig = None;
        let bytes = canonical_bytes(&self).map_err(|e| e.to_string())?;
        self.sig = Some(identity.sign_hex(&bytes));
        Ok(self)
    }
}

/// Accumulates until someone asks for it.
#[derive(Default)]
pub struct Meter {
    inner: Mutex<Counters>,
}

impl Meter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a finished job. `price` is what this node charges: split
    /// input/output/cache for OpenRouter, or a flat per-million / per-image.
    pub fn record(&self, result: &JobResult, price: &Price) {
        let mut c = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        c.requests += 1;

        match result.kind {
            JobKind::Image | JobKind::Video => {
                c.images += 1;
                c.revenue += price.amount;
            }
            JobKind::Llm => {
                // `tokens_measured` is true when the OpenAI tokenizer ran, or
                // when the provider stood behind a number. A frame-count
                // leftover from an older worker is still not billed.
                let measured = result
                    .meta
                    .get("tokens_measured")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if measured {
                    let usage = TokenUsage::from_meta(&result.meta).unwrap_or_default();
                    c.tokens_in += usage.prompt;
                    c.tokens_out += usage.completion;
                    c.tokens_cached += usage.cached;
                    c.revenue +=
                        price.charge_llm_micros(usage.prompt, usage.completion, usage.cached)
                            as f64
                            / 1_000_000.0;
                }
            }
        }
    }

    pub fn failed(&self) {
        let mut c = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        c.requests += 1;
        c.failures += 1;
    }

    /// A submission refused before it reached a backend. Not counted in
    /// `requests`: nothing was served, and rolling it in would make a node
    /// that turns away abuse look busy.
    pub fn rejected(&self) {
        let mut c = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        c.rejected += 1;
    }

    /// Take what has accumulated, leaving the meter at zero.
    pub fn drain(&self) -> Counters {
        let mut c = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *c)
    }

    /// Put counters back after a failed send, so an unreachable collector
    /// costs a delay rather than a hole in the chart.
    pub fn restore(&self, counters: Counters) {
        let mut c = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        c.requests += counters.requests;
        c.images += counters.images;
        c.tokens_in += counters.tokens_in;
        c.tokens_out += counters.tokens_out;
        c.tokens_cached += counters.tokens_cached;
        c.revenue += counters.revenue;
        c.failures += counters.failures;
        c.rejected += counters.rejected;
    }
}

/// Post `report` once. Separated from the loop so it can be tested against a
/// stub without waiting for a timer.
pub async fn send(http: &reqwest::Client, url: &str, report: &Report) -> Result<(), String> {
    let resp = http
        .post(url)
        .json(report)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(format!("HTTP {status}: {}", body.chars().take(200).collect::<String>()))
}

/// True when there is nothing worth sending. A node that served nothing still
/// reports on a slower cadence — a chart that cannot tell "idle" from "gone"
/// is missing the more interesting of the two.
pub fn nothing_happened(counters: &Counters) -> bool {
    counters.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootmode_core::PROTOCOL_VERSION;
    use uuid::Uuid;

    fn llm_result(prompt: u64, completion: u64, measured: bool) -> JobResult {
        JobResult {
            v: PROTOCOL_VERSION,
            job_id: Uuid::new_v4(),
            kind: JobKind::Llm,
            sha256: "x".into(),
            text: Some("hi".into()),
            tool_calls: Vec::new(),
            image_path_or_b64: None,
            thinking: None,
            meta: serde_json::json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "cached_tokens": if measured { 10 } else { 0 },
                "tokens_measured": measured,
            }),
        }
    }

    #[test]
    fn counts_tokens_the_server_stood_behind() {
        let meter = Meter::new();
        meter.record(&llm_result(1000, 500, true), &Price::new(0.60));
        let c = meter.drain();
        assert_eq!(
            (c.tokens_in, c.tokens_out, c.tokens_cached, c.requests),
            (1000, 500, 10, 1)
        );
        // 1,500 tokens at $0.60 per million. Cached is a subset of input,
        // already in that total — dropping it from the bill is how we lose
        // money against OpenRouter's cache-read charge.
        assert!((c.revenue - 0.0009).abs() < 1e-9, "{}", c.revenue);
    }

    #[test]
    fn a_guessed_token_count_is_not_published() {
        let meter = Meter::new();
        meter.record(&llm_result(0, 42, false), &Price::new(1.0));
        let c = meter.drain();
        // The request happened; the token count did not come from the server,
        // so it is left out rather than charted as though it had.
        assert_eq!(c.requests, 1);
        assert_eq!((c.tokens_in, c.tokens_out), (0, 0));
        assert_eq!(c.revenue, 0.0);
    }

    #[test]
    fn draining_empties_and_restoring_puts_it_back() {
        let meter = Meter::new();
        meter.record(&llm_result(10, 10, true), &Price::new(0.0));
        let taken = meter.drain();
        assert!(nothing_happened(&meter.drain()), "drain leaves it empty");

        meter.restore(taken);
        meter.record(&llm_result(5, 5, true), &Price::new(0.0));
        let c = meter.drain();
        // A failed send costs a delay, not the numbers.
        assert_eq!(c.requests, 2);
        assert_eq!(c.tokens_in, 15);
    }

    #[test]
    fn a_report_signs_and_verifies_over_its_own_body() {
        let identity = Identity::generate();
        let report = Report {
            v: 1,
            peer_id: identity.peer_id(),
            label: "box".into(),
            country: Some("DE".into()),
            caps: vec!["llm".into()],
            models: vec!["llama".into()],
            window_secs: 300,
            counters: Counters { requests: 3, ..Default::default() },
            currency: "USD".into(),
            sig: None,
        }
        .signed_by(&identity)
        .unwrap();

        let sig = report.sig.clone().unwrap();
        let bytes = canonical_bytes(&Report { sig: None, ..report.clone() }).unwrap();
        assert!(
            rootmode_core::identity::verify_hex(&report.peer_id, &bytes, &sig).is_ok(),
            "a collector can check the numbers came from this node"
        );

        // Someone else's key does not vouch for them.
        let other = Identity::generate();
        assert!(rootmode_core::identity::verify_hex(&other.peer_id(), &bytes, &sig).is_err());
    }
}
