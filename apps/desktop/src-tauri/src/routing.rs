//! Choosing who runs your work.
//!
//! You pick a *model*; the app picks the *provider*. That is the right way
//! round — you care what answers, not which box it ran on — but the choice is
//! never hidden: the provider and the price it advertised are shown with the
//! answer, and you can pin a provider if you would rather decide yourself.

use rand::Rng;
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
/// Ranking is cheapest; among providers at the same price the choice is
/// random, so equal offers share the load. Choosing the lowest latency was
/// the earlier rule, and it sent every user of a model to the same node —
/// the fastest to answer a probe is not the one with a free slot. A provider
/// that names no price is treated as free, because nothing is being charged
/// — pretending otherwise would push work away from the people running
/// nodes for nothing.
pub fn model_options(peers: &[Peer], kind: JobKind) -> Vec<ModelOption> {
    let mut options: Vec<ModelOption> = Vec::new();
    // How many providers have tied for the current best price of each model,
    // so a newcomer at that price replaces the holder with probability 1/n —
    // every tied provider ends up equally likely (reservoir sampling).
    let mut ties: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut rng = rand::thread_rng();

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
                unpriced: model.price.as_ref().map_or(true, |p| p.is_free()),
            };

            match options.iter_mut().find(|o| o.model == candidate.model) {
                Some(existing) => {
                    existing.providers += 1;
                    let take = if candidate.price < existing.price {
                        ties.insert(candidate.model.clone(), 1);
                        true
                    } else if candidate.price == existing.price {
                        let n = ties.entry(candidate.model.clone()).or_insert(1);
                        *n += 1;
                        rand::Rng::gen_range(&mut rng, 0..*n) == 0
                    } else {
                        false
                    };
                    if take {
                        let providers = existing.providers;
                        *existing = candidate;
                        existing.providers = providers;
                    }
                }
                None => {
                    ties.insert(candidate.model.clone(), 1);
                    options.push(candidate);
                }
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
    /// Video models: the shapes on offer and their rates. `price` is the
    /// default shape's; a chosen shape is quoted from this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<rootmode_core::VideoOffer>,
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
                    unpriced: model.price.as_ref().map_or(true, |p| p.is_free()),
                    latency_ms: peer.latency_ms,
                    video: model.video.clone(),
                })
        })
        .collect();

    // Cheapest first; equals by name, so the picker holds still between
    // refreshes. Latency is shown, not sorted on — the app does not steer
    // everyone to one node, and neither should the list's order.
    out.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
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

/// Who to try next when the first choice fails before saying anything.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fallback {
    pub peer_id: String,
    pub peer_label: String,
    pub model: String,
}

/// Providers worth trying, in order, after `first_peer_id` gave nothing.
///
/// The same model first, at the same price or less — cheapest, ties at
/// random — so a retry never costs more than the offer the user saw. When
/// the first choice was free, any other free provider of the kind follows,
/// on whatever model it serves: a free tier that is out of quota answers
/// from another free tier, not with an error. Money never enters silently
/// — a free choice is never retried on a paid one — and a swap of model
/// happens only between free text providers.
pub fn fallbacks(peers: &[Peer], kind: JobKind, model: &str, first_peer_id: &str) -> Vec<Fallback> {
    let first_price = peers
        .iter()
        .find(|p| p.id == first_peer_id)
        .and_then(|p| p.models.iter().find(|m| m.kind == kind && serves(m, model)))
        .map(|m| m.amount())
        .unwrap_or(0.0);
    let mut rng = rand::thread_rng();
    let mut same: Vec<(f64, u32, Fallback)> = Vec::new();
    let mut other_free: Vec<(u32, Fallback)> = Vec::new();
    for peer in peers.iter().filter(|p| p.status == "online" && p.id != first_peer_id) {
        for m in peer.models.iter().filter(|m| m.kind == kind) {
            let price = m.amount();
            let fallback = Fallback {
                peer_id: peer.id.clone(),
                peer_label: peer.label.clone(),
                model: m.id.clone(),
            };
            if serves(m, model) {
                if price <= first_price {
                    same.push((price, rng.gen(), fallback));
                }
            } else if first_price <= 0.0 && price <= 0.0 && kind == JobKind::Llm {
                other_free.push((rng.gen(), fallback));
            }
        }
    }
    same.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    other_free.sort_by_key(|f| f.0);
    same.into_iter()
        .map(|s| s.2)
        .chain(other_free.into_iter().map(|f| f.1))
        .collect()
}

/// The same loose match the job pipeline uses to price a request: a model
/// asked for by a longer name still belongs to the provider listing the
/// shorter one.
fn serves(m: &rootmode_core::ModelDescriptor, model: &str) -> bool {
    m.id == model || model.starts_with(&m.id)
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
            video: None,
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
    fn equal_prices_share_the_load() {
        // Three providers at one price: over many choices each gets picked,
        // and none is favoured for answering a probe faster.
        let peers = vec![
            peer("far", Some(200), vec![model("llama", Some(0.10))]),
            peer("near", Some(12), vec![model("llama", Some(0.10))]),
            peer("untimed", None, vec![model("llama", Some(0.10))]),
        ];
        let mut seen = std::collections::HashMap::new();
        for _ in 0..600 {
            let chosen = provider_for(&peers, JobKind::Llm, "llama").unwrap();
            assert_eq!(chosen.providers, 3);
            *seen.entry(chosen.peer_label).or_insert(0u32) += 1;
        }
        assert_eq!(seen.len(), 3, "every tied provider is chosen sometimes: {seen:?}");
        assert!(seen.values().all(|n| *n > 100), "roughly evenly: {seen:?}");
    }

    #[test]
    fn a_cheaper_provider_always_wins_the_tie_break_is_only_among_equals() {
        let peers = vec![
            peer("dear", Some(1), vec![model("llama", Some(0.20))]),
            peer("cheap-a", Some(500), vec![model("llama", Some(0.10))]),
            peer("cheap-b", None, vec![model("llama", Some(0.10))]),
        ];
        for _ in 0..50 {
            let chosen = provider_for(&peers, JobKind::Llm, "llama").unwrap();
            assert!(chosen.peer_label.starts_with("cheap-"), "{}", chosen.peer_label);
            assert_eq!(chosen.price, 0.10);
        }
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
            video: None,
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
    fn equal_prices_are_listed_by_name() {
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
            vec!["far", "near", "untimed"],
            "the picker holds still; latency is shown, not sorted on"
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

    fn labels(f: Vec<Fallback>) -> Vec<String> {
        f.into_iter().map(|f| f.peer_label).collect()
    }

    #[test]
    fn a_retry_never_costs_more_than_the_first_choice() {
        let peers = vec![
            peer("first", None, vec![model("llama", Some(1.0))]),
            peer("dearer", None, vec![model("llama", Some(2.0))]),
            peer("same", None, vec![model("llama", Some(1.0))]),
            peer("cheaper", None, vec![model("llama", Some(0.5))]),
            peer("other", None, vec![model("mistral", Some(0.1))]),
        ];
        assert_eq!(
            labels(fallbacks(&peers, JobKind::Llm, "llama", "id-first")),
            vec!["cheaper", "same"],
            "same model, no dearer, cheapest first; a paid choice never changes model"
        );
    }

    #[test]
    fn a_free_choice_moves_to_another_free_model_but_never_to_a_paid_one() {
        let peers = vec![
            peer("first", None, vec![model("gemma", Some(0.0))]),
            peer("paid-gemma", None, vec![model("gemma", Some(0.3))]),
            peer("free-inkling", None, vec![model("inkling", None)]),
            peer("paid-glm", None, vec![model("glm", Some(0.1))]),
        ];
        assert_eq!(
            labels(fallbacks(&peers, JobKind::Llm, "gemma", "id-first")),
            vec!["free-inkling"]
        );
    }

    #[test]
    fn a_free_twin_of_the_same_model_comes_before_other_free_models() {
        let peers = vec![
            peer("first", None, vec![model("gemma", None)]),
            peer("free-inkling", None, vec![model("inkling", None)]),
            peer("free-gemma", None, vec![model("gemma", None)]),
        ];
        assert_eq!(
            labels(fallbacks(&peers, JobKind::Llm, "gemma", "id-first")),
            vec!["free-gemma", "free-inkling"]
        );
    }

    #[test]
    fn offline_peers_and_the_first_choice_itself_are_not_fallbacks() {
        let mut down = peer("down", None, vec![model("llama", Some(0.1))]);
        down.status = "offline".into();
        let peers = vec![peer("first", None, vec![model("llama", Some(1.0))]), down];
        assert!(fallbacks(&peers, JobKind::Llm, "llama", "id-first").is_empty());
    }
}
