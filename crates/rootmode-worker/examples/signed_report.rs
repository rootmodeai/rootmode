//! Emit one signed stats report as JSON, for checking that the collector
//! agrees with this crate about canonical form and signatures.
//!
//!     cargo run -p rootmode-worker --example signed_report
//!
//! Piped into the collector's verifier, this is the test that catches the
//! failure that would otherwise only show up in production: two languages
//! serialising the same numbers differently, and every signature failing.

use rootmode_core::Identity;
use rootmode_worker::stats::{Counters, Report};

fn main() {
    let identity = Identity::generate();
    let report = Report {
        v: 1,
        peer_id: identity.peer_id(),
        label: "cross-language-check".into(),
        country: Some("DE".into()),
        caps: vec!["llm".into(), "image".into()],
        models: vec!["llama-3.1-70b".into(), "sdxl".into()],
        window_secs: 300,
        counters: Counters {
            requests: 128,
            images: 4,
            tokens_in: 1_234_567,
            tokens_out: 89_012,
            tokens_cached: 0,
            // Awkward on purpose: a float that does not survive a naive
            // round trip is exactly what would break signing.
            revenue: 0.1 + 0.2,
            failures: 2,
            rejected: 7,
        },
        currency: "USD".into(),
        sig: None,
    }
    .signed_by(&identity)
    .expect("sign");

    println!("{}", serde_json::to_string(&report).expect("serialise"));
}
