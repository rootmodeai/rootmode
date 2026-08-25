//! Choosing who runs your work.
//!
//! You pick a *model*; the app picks the *provider*. That is the right way
//! round — you care what answers, not which box it ran on — but the choice is
//! never hidden: the provider and the price it advertised are shown with the
//! answer, and you can pin a provider if you would rather decide yourself.

use rootmode_core::JobKind;
use serde::Serialize;

use crate::store::Peer;

/// One model you can ask for, and the provider that would serve it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelOption {
    pub model: String,
    pub kind: JobKind,
    /// How many online providers serve it.
    pub providers: u32,
    /// The one that would be used: cheapest, then fastest.
    pub peer_id: String,
    pub peer_label: String,
    /// Per million tokens for text, per image for images. `0` means free —
    /// either genuinely free or simply not priced.
    pub price: f64,
    pub currency: String,
    pub latency_ms: Option<u32>,
    /// True when nobody serving this model named a price.
    pub unpriced: bool,
}

/// Every model on offer, best provider first within each.
///
/// Ranking is cheapest, then lowest latency. A provider that names no price is
/// treated as free, because nothing is being charged — pretending otherwise
/// would push work away from the people running nodes for nothing.
pub fn model_options(peers: &[Peer], kind: JobKind) -> Vec<ModelOption> {
    let mut options: Vec<ModelOption> = Vec::new();

    for peer in peers.iter().filter(|p| p.status == "online") {
        for model in peer.models.iter().filter(|m| m.kind == kind) {
            let price = model.amount();
            let candidate = ModelOption {
                model: model.id.clone(),
                kind,
                providers: 1,
                peer_id: peer.id.clone(),
                peer_label: peer.label.clone(),
                price,
                currency: model
                    .price
                    .as_ref()
                    .map(|p| p.currency.clone())
                    .unwrap_or_else(|| "USD".to_string()),
                latency_ms: peer.latency_ms,
                unpriced: model.price.is_none(),
            };

            match options.iter_mut().find(|o| o.model == candidate.model) {
                Some(existing) => {
                    existing.providers += 1;
                    if beats(&candidate, existing) {
                        let providers = existing.providers;
                        *existing = candidate;
                        existing.providers = providers;
                    }
                }
                None => options.push(candidate),
            }
        }
    }

    // Cheapest models first, then alphabetical so the list does not jitter.
    options.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model.cmp(&b.model))
    });
    options
}

/// Cheaper wins. Equal price, lower latency wins. Unknown latency loses to
/// known, because a provider we have never timed is a provider we have never
/// successfully used.
fn beats(candidate: &ModelOption, current: &ModelOption) -> bool {
    if candidate.price != current.price {
        return candidate.price < current.price;
    }
    match (candidate.latency_ms, current.latency_ms) {
        (Some(a), Some(b)) => a < b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// One (model, provider) pair on offer.
///
/// [`model_options`] collapses to the best provider per model, which is what
/// you want when the app is choosing. This is the other view: everyone who
/// serves anything, so a person can look at the list and choose for
/// themselves — including choosing a dearer provider for a reason the app
/// cannot know, like having watched a cheaper one time out all morning.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderOption {
    pub model: String,
    pub kind: JobKind,
    pub peer_id: String,
    pub peer_label: String,
    /// Where the worker says it is. Part of choosing: latency is not the only
    /// reason to prefer one machine over another.
    pub peer_country: Option<String>,
    pub price: f64,
    pub currency: String,
    pub unpriced: bool,
    pub latency_ms: Option<u32>,
}

/// Everyone serving anything of this kind, cheapest first.
///
/// Sorted rather than left to the caller: "cheapest first" is the ordering the
/// price is *for*, and a list that arrives in peer-discovery order makes the
/// user do arithmetic the app already did.
pub fn provider_options(peers: &[Peer], kind: JobKind) -> Vec<ProviderOption> {
    let mut out: Vec<ProviderOption> = peers
        .iter()
        .filter(|p| p.status == "online")
        .flat_map(|peer| {
            peer.models
                .iter()
                .filter(|m| m.kind == kind)
                .map(move |model| ProviderOption {
                    model: model.id.clone(),
                    kind,
                    peer_id: peer.id.clone(),
                    peer_label: peer.label.clone(),
                    peer_country: peer.country.clone(),
                    price: model.amount(),
                    currency: model
                        .price
                        .as_ref()
                        .map(|p| p.currency.clone())
                        .unwrap_or_else(|| "USD".to_string()),
                    unpriced: model.price.is_none(),
                    latency_ms: peer.latency_ms,
                })
        })
        .collect();

    out.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
            // A provider we have never timed sorts last among equals: never
            // having timed it means never having successfully used it.
            .then_with(|| match (a.latency_ms, b.latency_ms) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.model.cmp(&b.model))
            .then_with(|| a.peer_label.cmp(&b.peer_label))
    });
    out
}

/// The provider to use for a named model, if anyone still serves it.
pub fn provider_for(peers: &[Peer], kind: JobKind, model: &str) -> Option<ModelOption> {
    model_options(peers, kind)
        .into_iter()
        .find(|o| o.model == model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootmode_core::{ModelDescriptor, Price};

    fn peer(label: &str, latency: Option<u32>, models: Vec<ModelDescriptor>) -> Peer {
        Peer {
            id: format!("id-{label}"),
            label: label.into(),
            country: None,
            endpoint: format!("p2p://{label}"),
            public_key: None,
            peer_id: None,
            status: "online".into(),
            latency_ms: latency,
            caps: vec!["llm".into()],
            models,
            max_concurrent: 1,
            last_seen: None,
            last_error: None,
            source: "discovered".into(),
            added_at: 0,
            payout: None,
        }
    }

    fn model(id: &str, price: Option<f64>) -> ModelDescriptor {
        ModelDescriptor {
            id: id.into(),
            sha256: None,
            kind: JobKind::Llm,
            price: price.map(Price::new),
        }
    }

    #[test]
    fn the_cheapest_provider_for_a_model_wins() {
        let peers = vec![
            peer("pricey", Some(10), vec![model("llama", Some(0.90))]),
            peer("cheap", Some(80), vec![model("llama", Some(0.10))]),
        ];

        let chosen = provider_for(&peers, JobKind::Llm, "llama").unwrap();
        assert_eq!(chosen.peer_label, "cheap");
        assert_eq!(chosen.providers, 2, "both are counted as serving it");
        assert_eq!(chosen.price, 0.10);
    }

    #[test]
    fn equal_price_is_broken_by_latency() {
        let peers = vec![
            peer("far", Some(200), vec![model("llama", Some(0.10))]),
            peer("near", Some(12), vec![model("llama", Some(0.10))]),
        ];
        assert_eq!(
            provider_for(&peers, JobKind::Llm, "llama")
                .unwrap()
                .peer_label,
            "near"
        );
    }

    #[test]
    fn a_provider_that_names_no_price_counts_as_free() {
        // Somebody running a node for nothing should not be sorted below
        // somebody charging for the same model.
        let peers = vec![
            peer("charges", Some(10), vec![model("llama", Some(0.10))]),
            peer("free", Some(90), vec![model("llama", None)]),
        ];

        let chosen = provider_for(&peers, JobKind::Llm, "llama").unwrap();
        assert_eq!(chosen.peer_label, "free");
        assert_eq!(chosen.price, 0.0);
        assert!(chosen.unpriced);
    }

    #[test]
    fn offline_providers_are_not_offered() {
        let mut offline = peer("gone", Some(5), vec![model("llama", Some(0.01))]);
        offline.status = "offline".into();
        let peers = vec![offline, peer("here", Some(50), vec![model("llama", None)])];

        let chosen = provider_for(&peers, JobKind::Llm, "llama").unwrap();
        assert_eq!(chosen.peer_label, "here");
        assert_eq!(chosen.providers, 1);
    }

    #[test]
    fn models_are_listed_cheapest_first_and_only_of_the_kind_asked_for() {
        let image = ModelDescriptor {
            id: "sdxl".into(),
            sha256: None,
            kind: JobKind::Image,
            price: Some(Price::new(0.02)),
        };
        let peers = vec![peer(
            "mixed",
            Some(10),
            vec![
                model("expensive", Some(2.0)),
                model("budget", Some(0.2)),
                image,
            ],
        )];

        let text: Vec<String> = model_options(&peers, JobKind::Llm)
            .into_iter()
            .map(|o| o.model)
            .collect();
        assert_eq!(text, vec!["budget", "expensive"]);

        let images: Vec<String> = model_options(&peers, JobKind::Image)
            .into_iter()
            .map(|o| o.model)
            .collect();
        assert_eq!(images, vec!["sdxl"]);
    }

    #[test]
    fn listed_prices_round_up_to_cents() {
        let peers = vec![peer(
            "or",
            Some(10),
            vec![model("llama", Some(0.141))],
        )];
        let rows = provider_options(&peers, JobKind::Llm);
        assert_eq!(rows[0].price, 0.15);
    }

    #[test]
    fn every_provider_is_listed_not_just_the_best_one() {
        // The picker is for choosing by hand, so it must show the dearer
        // provider too — collapsing to the cheapest is the app deciding.
        let peers = vec![
            peer("pricey", Some(10), vec![model("llama", Some(0.90))]),
            peer("cheap", Some(80), vec![model("llama", Some(0.10))]),
            peer("free", Some(50), vec![model("llama", None)]),
        ];

        let rows = provider_options(&peers, JobKind::Llm);
        assert_eq!(rows.len(), 3, "one row per provider, not per model");
        assert_eq!(
            rows.iter()
                .map(|r| r.peer_label.as_str())
                .collect::<Vec<_>>(),
            vec!["free", "cheap", "pricey"],
            "cheapest first, and unpriced counts as free"
        );
    }

    #[test]
    fn equal_prices_are_ordered_by_latency_then_name() {
        let peers = vec![
            peer("far", Some(300), vec![model("llama", Some(0.5))]),
            peer("near", Some(9), vec![model("llama", Some(0.5))]),
            peer("untimed", None, vec![model("llama", Some(0.5))]),
        ];
        assert_eq!(
            provider_options(&peers, JobKind::Llm)
                .iter()
                .map(|r| r.peer_label.as_str())
                .collect::<Vec<_>>(),
            vec!["near", "far", "untimed"],
            "a provider never timed is one never successfully used"
        );
    }

    #[test]
    fn offline_providers_are_not_in_the_picker() {
        let mut gone = peer("gone", Some(5), vec![model("llama", Some(0.01))]);
        gone.status = "offline".into();
        let peers = vec![gone, peer("here", Some(50), vec![model("llama", None)])];

        let rows = provider_options(&peers, JobKind::Llm);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].peer_label, "here");
    }

    #[test]
    fn nothing_online_means_nothing_on_offer() {
        assert!(model_options(&[], JobKind::Llm).is_empty());
        assert!(provider_for(&[], JobKind::Llm, "llama").is_none());
    }
}
