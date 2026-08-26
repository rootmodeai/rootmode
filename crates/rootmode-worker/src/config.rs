//! Worker configuration.
//!
//! Everything the operator controls lives here, in one TOML file. Nothing in a
//! job message can change any of it — a client picks *values*, never targets,
//! paths, or workflows.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rand::RngCore;

use serde::{Deserialize, Serialize};

use crate::error::{Result, WorkerError};

/// Inputs of a workflow the worker will write into.
///
/// Deliberately two. Everything else about a render — steps, guidance, size,
/// scheduler, negative prompt — is baked into the graph the operator exported,
/// because that is where the knowledge lives. A client asking for a picture
/// cannot know the right guidance scale for a checkpoint it has never seen,
/// and should not be made to guess.
///
/// * `prompt` — the only thing the client supplies.
/// * `seed` — supplied by the *worker*, not the client, so repeated prompts
///   do not return the identical picture. Leave it undeclared and every
///   render uses whatever seed the graph was saved with, which is a valid
///   choice if you want a fixed output.
pub const ALLOWED_SLOTS: [&str; 2] = ["prompt", "seed"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub worker: WorkerConfig,
    #[serde(default)]
    pub stats: StatsConfig,
    #[serde(default)]
    pub p2p: P2pConfig,
    #[serde(default)]
    pub payments: PaymentsConfig,
    #[serde(default, rename = "backends")]
    pub backends: Vec<BackendConfig>,
}

/// Reporting to a stats collector.
///
/// On by default, pointed at the network's own collector, because an explorer
/// with nothing in it is worse than no explorer: a network nobody can see the
/// size of looks dead.
///
/// It is the one part of a worker that talks to something other than a peer,
/// so it is kept narrow — counts of what this node served, signed with its
/// key, never a prompt, never a result, never who asked — and it is one line
/// to turn off:
///
/// ```toml
/// [stats]
/// url = ""    # report to nobody
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsConfig {
    /// Where to POST reports. Empty disables reporting entirely.
    #[serde(default = "default_stats_url")]
    pub url: String,
    /// How often, in seconds.
    #[serde(default = "default_report_secs")]
    pub interval_secs: u64,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            url: default_stats_url(),
            interval_secs: default_report_secs(),
        }
    }
}

impl StatsConfig {
    pub fn enabled(&self) -> bool {
        !self.url.trim().is_empty()
    }
}

fn default_report_secs() -> u64 {
    300
}

/// The network's collector, which is the site itself — one host, one
/// certificate, no separate stats service to stand up. Change it to run your
/// own, or empty it to report to nobody: a worker that reports nothing serves
/// jobs exactly the same.
fn default_stats_url() -> String {
    "https://rootmode.ai/report".to_string()
}

/// Joining the network. With no bootstrap addresses the worker is still fully
/// usable — clients just have to be told its `ws://` address by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2pConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Entry points, `/ip4/…/tcp/4001/p2p/<peer id>`. Empty = no discovery.
    #[serde(default)]
    pub bootstrap: Vec<String>,
    /// Addresses to listen on for peer connections.
    #[serde(default = "default_p2p_listen")]
    pub listen: Vec<String>,
    /// Ask a bootstrap node to relay for us. Needed when this box cannot
    /// accept an inbound connection — behind NAT, no port forwarding.
    #[serde(default = "default_true")]
    pub relay: bool,
    /// Serve DHT queries for other nodes. Turn on when this host has a public
    /// address and you want to help hold the network up.
    #[serde(default)]
    pub dht_server: bool,
    /// Announce on the local network so machines on the same LAN find this
    /// worker with nothing configured. Independent of the DHT.
    #[serde(default = "default_true")]
    pub local_discovery: bool,
    /// Addresses to advertise, when they are not the ones this process binds:
    /// a container with published ports, a static NAT mapping, a public IP in
    /// front of a private one. e.g. `/ip4/203.0.113.10/tcp/4101`.
    #[serde(default)]
    pub external: Vec<String>,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bootstrap: Vec::new(),
            listen: default_p2p_listen(),
            relay: true,
            dht_server: false,
            local_discovery: true,
            external: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_p2p_listen() -> Vec<String> {
    vec!["/ip4/0.0.0.0/tcp/4101".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Shown to clients in the announce. Not an identity — the key is.
    #[serde(default = "default_label")]
    pub label: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Jobs run at once. Match this to what your GPUs can actually hold.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Refuse unsigned submissions. Off by default so a first run works;
    /// Verify the ed25519 signature on every submission.
    ///
    /// On by default. Every job already carries a signature, so this costs
    /// nothing and means each request is attributable to a key — which is what
    /// makes an allowlist, a block, or an abuse report possible at all. An
    /// anonymous request cannot be refused twice.
    ///
    /// Turn it off only for a node on a network you control, talking to
    /// clients you know are not signing.
    #[serde(default = "default_true")]
    pub require_signature: bool,
    /// Client peer ids allowed to submit. Empty means anyone who can reach
    /// the port. Setting this implies `require_signature`.
    #[serde(default)]
    pub allow_peers: Vec<String>,
    /// Where this machine is, as an ISO 3166-1 alpha-2 code — "DE", "GB",
    /// "SG". Shown to clients beside the node's name.
    ///
    /// Declared by you, not looked up: geolocating the address would mean
    /// somebody's database learning who is on this network. Empty means the
    /// client shows nothing rather than guessing.
    #[serde(default)]
    pub country: String,
    /// ed25519 seed file. Generated on first run if absent.
    #[serde(default = "default_identity_file")]
    pub identity_file: PathBuf,
    /// Where this node is paid, when there is anything to pay — an address on
    /// the settlement chain (Base). Advertised to clients and written into
    /// every receipt.
    ///
    /// Empty means this node takes no money: it will still serve, and any
    /// spending authorisation a client attaches is simply ignored.
    #[serde(default)]
    pub payout_address: String,
    /// How often to ask the backends what models they have now, in seconds.
    ///
    /// Load another model into vLLM, or drop a checkpoint into ComfyUI, and it
    /// becomes servable within this long — no restart. `0` disables polling,
    /// for a node whose model set never changes.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
}

fn default_label() -> String {
    "rootmode worker".into()
}
fn default_listen() -> String {
    "0.0.0.0:9944".into()
}
fn default_max_concurrent() -> u32 {
    1
}
fn default_refresh_secs() -> u64 {
    60
}
fn default_identity_file() -> PathBuf {
    PathBuf::from("worker.key")
}
fn default_timeout() -> u64 {
    900
}

fn default_currency() -> String {
    "USD".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BackendConfig {
    /// Any OpenAI-compatible chat completions server: vLLM, SGLang,
    /// llama.cpp's server, TGI's OpenAI shim.
    Vllm(VllmConfig),
    Comfyui(ComfyConfig),
    /// Seed capacity: answers by forwarding to OpenRouter. See
    /// [`crate::backends::openrouter`].
    Openrouter(OpenRouterConfig),
}

/// A node that serves real models without owning a GPU.
///
/// Indistinguishable from a hardware node on the wire, on purpose — it exists
/// so the network is not empty before real workers arrive, and each one can be
/// switched off as they do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterConfig {
    /// An OpenRouter key. Required.
    pub api_key: String,
    /// The handful of models this node claims to hold. Names may be short
    /// (`llama-3.3-70b-instruct`) or OpenRouter's own
    /// (`meta-llama/llama-3.3-70b-instruct`).
    ///
    /// Give each node a different few. One machine does not hold a hundred
    /// checkpoints, and a node that advertised the whole catalogue would be
    /// both obvious and useless to route against.
    #[serde(default)]
    pub models: Vec<String>,
    /// Multiplier on OpenRouter's published price. 1.0 charges exactly what
    /// they do; 1.15 is 15% above catalogue. Unset is 1.0.
    #[serde(default = "default_markup")]
    pub markup: f64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_markup() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VllmConfig {
    /// Base URL, e.g. `http://127.0.0.1:8000`. `/v1/...` is appended.
    pub endpoint: String,
    /// Sent as `Authorization: Bearer`. Only needed if you started vLLM with
    /// `--api-key`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Models to advertise. Empty means "ask the server" via `/v1/models`.
    #[serde(default)]
    pub models: Vec<String>,
    /// Optional sha256 per model id, for operators who can attest to weights.
    #[serde(default)]
    pub model_hashes: BTreeMap<String, String>,
    /// What you charge per million tokens, for every model this backend
    /// advertises. A per-id entry in `prices` wins. Unset, and nothing in
    /// `prices`, is advertised as free. Nothing settles this yet — clients
    /// use it to choose between providers.
    #[serde(default)]
    pub price: Option<f64>,
    /// Per-model overrides of `price`, keyed by the id `/v1/models` reports.
    #[serde(default)]
    pub prices: BTreeMap<String, f64>,
    /// Currency for `price` / `prices`.
    #[serde(default = "default_currency")]
    pub currency: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

/// One model, and the graph that renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowChoice {
    /// What a client asks for. Matched case-insensitively, and by prefix, so
    /// `krea2` finds `krea2-turbo`.
    pub model: String,
    /// The API-format export. **Save (API Format)**, not the editor's save.
    pub file: PathBuf,
    /// Which inputs the worker fills in *this* graph. Node ids differ between
    /// workflows, so a shared default would write the prompt into whatever
    /// node 6 happens to be. Left out, the standard positions are assumed and
    /// checked at startup like any other slot.
    #[serde(default)]
    pub slots: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComfyConfig {
    /// Base URL, e.g. `http://127.0.0.1:8188`.
    pub endpoint: String,
    /// A ComfyUI workflow exported in **API format** (Save (API Format) in the
    /// web UI). This is the only graph this worker will ever run.
    ///
    /// Optional. Left out, the worker builds a standard text-to-image graph
    /// from what the server reports it can do — which is all most operators
    /// need, and means pointing at an endpoint is the whole configuration.
    /// Supply one when your pipeline is not the standard one: LoRAs, ControlNet,
    /// upscalers, anything with a shape of its own.
    #[serde(default)]
    pub workflow: Option<PathBuf>,
    /// A workflow per model, for a box serving several pipelines.
    ///
    /// One graph cannot serve every checkpoint: an all-in-one SDXL file has a
    /// text encoder inside it, a Flux-style one wants `CLIPLoader` nodes
    /// feeding it, and a video model looks like neither. So each entry names
    /// the model a client asks for and the graph that runs it:
    ///
    /// ```toml
    /// [[backends.workflow_for]]
    /// model = "krea2-turbo"
    /// file  = "/etc/rootmode/workflows/krea2.json"
    ///
    /// [[backends.workflow_for]]
    /// model = "lustify-v7"
    /// file  = "/etc/rootmode/workflows/lustify.json"
    /// ```
    ///
    /// These are advertised alongside the checkpoints the built-in graph can
    /// serve, so a client picks a model and gets the pipeline built for it.
    #[serde(default)]
    pub workflow_for: Vec<WorkflowChoice>,
    /// The name clients see and ask for.
    ///
    /// Optional: left out, the worker uses whatever checkpoint the server has
    /// installed. Set it to advertise a friendlier name than the filename, or
    /// to choose between several.
    #[serde(default)]
    pub checkpoint_id: String,
    #[serde(default)]
    pub model_hash: Option<String>,
    /// What you charge per image, for every model this backend advertises.
    /// A per-id entry in `prices` wins. Unset, and nothing in `prices`, is
    /// advertised as free.
    #[serde(default)]
    pub price: Option<f64>,
    /// Per-model overrides of `price`, keyed by the name the worker advertises
    /// (`sdxl`, `krea2-turbo`). Matched case-insensitively, and by prefix, so
    /// `krea2` prices `krea2-turbo`.
    #[serde(default)]
    pub prices: BTreeMap<String, f64>,
    #[serde(default = "default_currency")]
    pub currency: String,
    /// Which node inputs the worker fills, e.g. `prompt = "6.inputs.text"`.
    /// Only [`ALLOWED_SLOTS`] are accepted; everything else in the graph is
    /// yours and is used exactly as you exported it.
    ///
    /// Empty with a generated graph means the built-in slots. Empty with your
    /// own workflow means nothing is fillable — the same picture every time —
    /// so that combination is refused rather than silently useless.
    #[serde(default)]
    pub slots: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

/// Substitute `${VAR}` from the environment.
///
/// So an API key can be handed to the process rather than committed next to
/// the label and the port. An unset variable is left as-is rather than blanked:
/// a config that fails validation naming the variable is easier to fix than one
/// that silently authenticates as nobody.
fn expand_env(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => {
                        tracing::warn!("config refers to ${{{name}}}, which is not set");
                        out.push_str(&rest[start..start + 2 + end + 1]);
                    }
                }
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| WorkerError::Config(format!("cannot read {}: {e}", path.display())))?;
        let mut config: Config =
            toml::from_str(&expand_env(&raw)).map_err(|e| WorkerError::Config(e.to_string()))?;

        // Relative paths are relative to the config file, not the working
        // directory — operators run this from systemd, cron, and a shell.
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            config.worker.identity_file = resolve(dir, &config.worker.identity_file);
            if !config.payments.key_file.as_os_str().is_empty() {
                config.payments.key_file = resolve(dir, &config.payments.key_file);
            }
            if !config.payments.channels_file.as_os_str().is_empty() {
                config.payments.channels_file = resolve(dir, &config.payments.channels_file);
            }
            for backend in &mut config.backends {
                if let BackendConfig::Comfyui(c) = backend {
                    c.workflow = c.workflow.as_ref().map(|w| resolve(dir, w));
                }
            }
        }

        config.apply_payment_env();
        config.apply_network_defaults();
        config.payments.apply_key();
        config.apply_payout_default();
        config.validate()?;
        Ok(config)
    }

    /// Whether this node means to be paid: it names a payout, or a backend
    /// carries a price. A node with neither serves free and never touches
    /// the chain, whatever else is configured.
    pub fn charges(&self) -> bool {
        if !self.worker.payout_address.trim().is_empty() {
            return true;
        }
        self.backends.iter().any(|b| match b {
            BackendConfig::Vllm(c) => c.price.is_some() || !c.prices.is_empty(),
            BackendConfig::Comfyui(c) => c.price.is_some() || !c.prices.is_empty(),
            BackendConfig::Openrouter(_) => true,
        })
    }

    /// A priced node settles on the network's pot. The address, chain and a
    /// public RPC are built into the binary from the same deploy record the
    /// desktop ships with, so an operator names a price and a wallet and
    /// nothing else — a contract address is not something to type. Anything
    /// set explicitly (file or environment) wins, and a node on another
    /// chain (a local Anvil, a testnet) gets no default at all.
    fn apply_network_defaults(&mut self) {
        if !self.charges() {
            return;
        }
        let Some(chain) = bundled_chain() else {
            return;
        };
        if chain.chain_id != self.payments.chain_id {
            return;
        }
        if self.payments.contract.trim().is_empty() {
            self.payments.contract = chain.pot;
        }
        if self.payments.rpc.trim().is_empty() {
            self.payments.rpc = chain.rpc;
        }
    }

    /// With no payout named, earnings go to the node's own settle key — the
    /// key on its volume that the operator already has to fund for gas. That
    /// is a working default, not a silent one: it is logged on every start,
    /// and the docs say to name a wallet of your own before it adds up.
    fn apply_payout_default(&mut self) {
        if !self.worker.payout_address.trim().is_empty() || !self.charges() {
            return;
        }
        let sender = self.payments.sender.trim();
        if sender.is_empty() {
            return;
        }
        self.worker.payout_address = sender.to_string();
        tracing::warn!(
            "no ROOTMODE_PAYOUT set: earnings go to this node's own settle key {sender} (on its \
             volume). Set ROOTMODE_PAYOUT to a wallet you control to send them elsewhere"
        );
    }

    fn apply_payment_env(&mut self) {
        fn scrub(s: &mut String) {
            if s.trim().is_empty() || s.trim().starts_with("${") {
                s.clear();
            }
        }
        scrub(&mut self.payments.contract);
        scrub(&mut self.payments.rpc);
        scrub(&mut self.worker.payout_address);
        if self.payments.contract.is_empty() {
            if let Ok(v) = std::env::var("ROOTMODE_POT") {
                self.payments.contract = v;
            }
        }
        if self.payments.rpc.is_empty() {
            if let Ok(v) = std::env::var("ROOTMODE_RPC") {
                self.payments.rpc = v;
            }
        }
        if self.worker.payout_address.is_empty() {
            if let Ok(v) = std::env::var("ROOTMODE_PAYOUT") {
                self.worker.payout_address = v;
            }
        }
        if let Ok(v) = std::env::var("ROOTMODE_CHAIN_ID") {
            if let Ok(id) = v.trim().parse::<u64>() {
                self.payments.chain_id = id;
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.worker
            .listen
            .parse::<SocketAddr>()
            .map_err(|e| WorkerError::Config(format!("listen '{}': {e}", self.worker.listen)))?;

        if self.worker.max_concurrent == 0 {
            return Err(WorkerError::Config(
                "max_concurrent must be at least 1".into(),
            ));
        }

        for peer in &self.worker.allow_peers {
            if peer.len() != 64 || hex::decode(peer).is_err() {
                return Err(WorkerError::Config(format!(
                    "allow_peers entry '{peer}' is not a 64-character hex peer id"
                )));
            }
        }

        for addr in &self.p2p.bootstrap {
            rootmode_p2p::parse_bootstrap(addr).map_err(|e| WorkerError::Config(e.to_string()))?;
        }
        for addr in &self.p2p.listen {
            addr.parse::<rootmode_p2p::Multiaddr>()
                .map_err(|e| WorkerError::Config(format!("p2p listen address '{addr}': {e}")))?;
        }

        if self.backends.is_empty() {
            return Err(WorkerError::Config(
                "no backends configured — this worker would advertise nothing".into(),
            ));
        }

        for backend in &self.backends {
            match backend {
                BackendConfig::Vllm(c) => check_url("vllm", &c.endpoint)?,
                BackendConfig::Openrouter(c) => {
                    if c.api_key.trim().is_empty() {
                        return Err(WorkerError::Config(
                            "openrouter backend needs an api_key".into(),
                        ));
                    }
                    if c.models.is_empty() {
                        return Err(WorkerError::Config(
                            "openrouter backend needs a `models` list — a node that advertises \
                             the whole catalogue fools nobody".into(),
                        ));
                    }
                }
                BackendConfig::Comfyui(c) => {
                    check_url("comfyui", &c.endpoint)?;
                    if let Some(workflow) = &c.workflow {
                        if !workflow.exists() {
                            return Err(WorkerError::Config(format!(
                                "comfyui workflow not found: {}",
                                workflow.display()
                            )));
                        }
                        // Your graph, your slot map: nothing can be guessed
                        // about where the prompt goes in a pipeline we have
                        // never seen.
                        if c.slots.is_empty() {
                            return Err(WorkerError::Config(
                                "comfyui backend names a workflow but no [backends.slots] — \
                                 a client could not set the prompt, so every render would be \
                                 the same picture"
                                    .into(),
                            ));
                        }
                    }
                    for (field, path) in &c.slots {
                        if !ALLOWED_SLOTS.contains(&field.as_str()) {
                            return Err(WorkerError::Config(format!(
                                "unknown slot '{field}' (allowed: {})",
                                ALLOWED_SLOTS.join(", ")
                            )));
                        }
                        if path.split('.').count() < 2 {
                            return Err(WorkerError::Config(format!(
                                "slot '{field}' path '{path}' should look like '6.inputs.text'"
                            )));
                        }
                    }
                    // Only meaningful for a workflow the operator wrote: a
                    // generated graph gets the built-in slots, which always
                    // include the prompt.
                    if c.workflow.is_some() && !c.slots.contains_key("prompt") {
                        return Err(WorkerError::Config(
                            "comfyui slots must include 'prompt'".into(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Signature checking is implied by an allowlist: you cannot enforce
    /// "these peers only" without verifying who is speaking.
    pub fn require_signature(&self) -> bool {
        self.worker.require_signature || !self.worker.allow_peers.is_empty()
    }
}

fn resolve(dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    }
}

fn check_url(what: &str, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| WorkerError::Config(format!("{what} endpoint '{url}': {e}")))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(WorkerError::Config(format!(
            "{what} endpoint must be http or https (got {other})"
        ))),
    }
}

/// Printed by `rootmode-worker init`.
pub const EXAMPLE_CONFIG: &str = r#"# rootmode worker
#
# One file, one node. Nothing a client sends can change anything below.

[worker]
label = "dgx-spark-0"
listen = "0.0.0.0:9944"

# Jobs to run at once. Match this to what the box can actually hold.
max_concurrent = 2

# Refuse unsigned submissions. Turn this on once your clients sign.
require_signature = false

# Client peer ids allowed to submit. Empty = anyone who can reach the port.
# Setting this implies require_signature.
allow_peers = []

# ed25519 seed. Generated on first run, chmod 0600. Back it up: it is this
# node's identity on the network.
identity_file = "worker.key"


# --- joining the network -----------------------------------------------------
# Without a bootstrap address this worker still works — clients just have to be
# given its ws:// address by hand. With one, it announces what it serves and
# clients discover it.
[p2p]
enabled = true
# Empty means the entry points compiled into the build. Set this only to
# override them, e.g. for a private network.
bootstrap = []
listen = ["/ip4/0.0.0.0/tcp/4101"]

# Ask a bootstrap node to relay for us. Leave on if this box is behind NAT.
relay = true

# Help hold the network up by answering DHT queries. Turn on only if this host
# has a public address.
dht_server = false

# Announce on the local network too, so machines on the same LAN find this
# worker without any address at all.
local_discovery = true

# Addresses to advertise when they are not what this process binds to — a
# container with published ports, a static NAT mapping, a public IP in front of
# a private one. Without this, peers discover you and then cannot dial you.
# external = ["/ip4/203.0.113.10/tcp/4101"]


# --- LLM inference -----------------------------------------------------------
# Any OpenAI-compatible server: vLLM, SGLang, llama.cpp, TGI's shim.
[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
# api_key = "..."            # only if you started vLLM with --api-key
# models = []                # empty = ask the server via /v1/models
#
# What you charge, per million tokens. One number covers every model this
# server reports. Anything unpriced is advertised as free, and clients pick
# the cheapest provider for the model they want.
# price = 0.15
# currency = "USD"
# [backends.prices]
# "meta-llama/Llama-3.1-8B-Instruct" = 0.40
# [backends.model_hashes]    # optional, if you can attest to the weights
# "meta-llama/Llama-3.1-8B-Instruct" = "sha256hex..."


# --- Image generation --------------------------------------------------------
# The workflow is exported from ComfyUI with "Save (API Format)". It is the
# only graph this worker will ever run: a client fills the slots below and
# nothing else.
#
# [[backends]]
# kind = "comfyui"
# endpoint = "http://127.0.0.1:8188"
# workflow = "workflows/sdxl_txt2img.json"
# checkpoint_id = "sdxl-base-1.0"
# price = 0.02                   # per image, every checkpoint; unset is free
# [backends.prices]
# "krea2-turbo" = 0.08
#
# # The client sends a prompt and nothing else. Everything else about the
# # render is whatever you saved in the workflow. Declare `seed` so repeated
# # prompts vary; leave it out to always produce the same picture.
# [backends.slots]
# prompt = "6.inputs.text"
# seed   = "3.inputs.seed"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("rootmode-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_secret_can_come_from_the_environment_rather_than_the_file() {
        std::env::set_var("ROOTMODE_TEST_KEY", "sk-or-secret");
        assert_eq!(expand_env("api_key = \"${ROOTMODE_TEST_KEY}\""), "api_key = \"sk-or-secret\"");

        // Left verbatim when unset, so validation complains about a key that
        // is obviously a placeholder instead of one that is empty.
        assert_eq!(expand_env("${NOT_SET_ANYWHERE}"), "${NOT_SET_ANYWHERE}");
        assert_eq!(expand_env("no substitutions here"), "no substitutions here");
        assert_eq!(expand_env("${unterminated"), "${unterminated");
    }

    #[test]
    fn a_seed_node_reads_as_an_ordinary_worker() {
        let config: Config = toml::from_str(
            r#"
[worker]
label = "atlas"

[[backends]]
kind = "openrouter"
api_key = "sk-or-test"
models = ["llama-3.3-70b-instruct", "qwen3-coder"]
"#,
        )
        .unwrap();
        config.validate().unwrap();

        let BackendConfig::Openrouter(c) = &config.backends[0] else {
            panic!("expected an openrouter backend");
        };
        assert_eq!(c.models.len(), 2);
        // Unset markup is catalogue price. Seed nodes set 1.15 explicitly.
        assert_eq!(c.markup, 1.0);

        let marked: Config = toml::from_str(
            r#"
[worker]
label = "atlas"

[[backends]]
kind = "openrouter"
api_key = "sk-or-test"
markup = 1.15
models = ["llama-3.3-70b-instruct"]
"#,
        )
        .unwrap();
        let BackendConfig::Openrouter(c) = &marked.backends[0] else {
            panic!("expected an openrouter backend");
        };
        assert_eq!(c.markup, 1.15);
    }

    #[test]
    fn a_seed_node_must_say_which_models_it_holds() {
        // Advertising the whole catalogue would be both implausible and
        // useless to route against, so it is a config error rather than a
        // shrug at runtime.
        let config: Config = toml::from_str(
            r#"
[worker]
label = "atlas"

[[backends]]
kind = "openrouter"
api_key = "sk-or-test"
"#,
        )
        .unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("models"), "{err}");

        let config: Config = toml::from_str(
            r#"
[worker]
label = "atlas"

[[backends]]
kind = "openrouter"
api_key = ""
models = ["llama-3.3-70b-instruct"]
"#,
        )
        .unwrap();
        assert!(config.validate().unwrap_err().to_string().contains("api_key"));
    }

    #[test]
    fn a_comfyui_backend_needs_nothing_but_an_endpoint() {
        // The whole point: point it at ComfyUI and it works. Requiring a
        // hand-written graph for the ordinary case put the operator's pipeline
        // in the client's configuration, which is backwards.
        let config: Config = toml::from_str(
            r#"
[worker]
identity_file = "w.key"

[[backends]]
kind = "comfyui"
endpoint = "http://127.0.0.1:8188"
"#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn a_workflow_of_your_own_still_needs_its_slots() {
        // Nothing can be guessed about where the prompt goes in a graph we
        // have never seen, and a workflow with no fillable prompt renders the
        // same picture every time.
        let dir = std::env::temp_dir().join(format!("rootmode-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let workflow = dir.join("wf.json");
        std::fs::write(&workflow, "{}").unwrap();

        let config: Config = toml::from_str(&format!(
            r#"
[worker]
identity_file = "w.key"

[[backends]]
kind = "comfyui"
endpoint = "http://127.0.0.1:8188"
workflow = "{}"
"#,
            workflow.display()
        ))
        .unwrap();

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("slots"), "got: {err}");
    }

    #[test]
    fn a_priced_node_settles_on_the_networks_pot_without_being_told_the_address() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
listen = "0.0.0.0:9944"
identity_file = "worker.key"

[payments]
key_file = "pay.key"

[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
price = 0.15
"#,
        );
        let config = Config::load(&p).unwrap();
        assert!(config.charges());
        assert_eq!(
            config.payments.contract,
            bundled_pot().expect("chain.base.json names the pot"),
            "the pot comes from the bundled deploy record"
        );
        assert!(!config.payments.rpc.is_empty(), "and so does an RPC");
        assert_eq!(config.payments.chain_id, 8453);
        // No payout named: earnings go to the settle key minted on the volume.
        assert!(!config.payments.sender.is_empty(), "a pay key was minted");
        assert_eq!(config.worker.payout_address, config.payments.sender);
    }

    #[test]
    fn a_free_node_stays_off_the_chain() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
listen = "0.0.0.0:9944"
identity_file = "worker.key"

[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
"#,
        );
        let config = Config::load(&p).unwrap();
        assert!(!config.charges());
        assert!(config.payments.contract.is_empty(), "no price, no pot");
        assert!(config.worker.payout_address.is_empty());
    }

    #[test]
    fn a_node_on_another_chain_gets_no_default_pot() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
listen = "0.0.0.0:9944"
identity_file = "worker.key"
payout_address = "0x000000000000000000000000000000000000dEaD"

[payments]
chain_id = 31337

[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
price = 0.15
"#,
        );
        let config = Config::load(&p).unwrap();
        assert!(config.payments.contract.is_empty(), "Anvil is not Base");
    }

    #[test]
    fn example_config_is_valid() {
        let d = tmpdir();
        let p = write(&d, "worker.toml", EXAMPLE_CONFIG);
        let config = Config::load(&p).unwrap();
        assert_eq!(config.worker.max_concurrent, 2);
        assert_eq!(
            config.backends.len(),
            1,
            "image backend is commented out by default"
        );
        assert!(!config.require_signature());
    }

    #[test]
    fn a_comfyui_backend_can_price_each_checkpoint() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
[[backends]]
kind = "comfyui"
endpoint = "http://127.0.0.1:8188"
price = 0.02
[backends.prices]
"flux1-dev" = 0.05
"#,
        );
        let config = Config::load(&p).unwrap();
        match &config.backends[0] {
            BackendConfig::Comfyui(c) => {
                assert_eq!(c.price, Some(0.02));
                assert_eq!(c.prices.get("flux1-dev"), Some(&0.05));
            }
            other => panic!("expected comfyui, got {other:?}"),
        }
    }

    #[test]
    fn a_vllm_backend_can_name_one_price_for_every_model() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
price = 0.15
[backends.prices]
"mixtral" = 0.40
"#,
        );
        let config = Config::load(&p).unwrap();
        match &config.backends[0] {
            BackendConfig::Vllm(v) => {
                assert_eq!(v.price, Some(0.15));
                assert_eq!(v.prices.get("mixtral"), Some(&0.40));
            }
            other => panic!("expected vllm, got {other:?}"),
        }
    }

    #[test]
    fn paths_resolve_against_the_config_file() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
identity_file = "keys/worker.key"
[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
"#,
        );
        let config = Config::load(&p).unwrap();
        assert_eq!(config.worker.identity_file, d.join("keys/worker.key"));
    }

    #[test]
    fn an_allowlist_implies_signature_checking() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            &format!(
                r#"
[worker]
allow_peers = ["{}"]
[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
"#,
                "ab".repeat(32)
            ),
        );
        assert!(Config::load(&p).unwrap().require_signature());
    }

    #[test]
    fn rejects_undeclared_slots() {
        let d = tmpdir();
        write(&d, "wf.json", "{}");
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
[[backends]]
kind = "comfyui"
endpoint = "http://127.0.0.1:8188"
workflow = "wf.json"
checkpoint_id = "sdxl"
[backends.slots]
prompt = "6.inputs.text"
ckpt_name = "4.inputs.ckpt_name"
"#,
        );
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("unknown slot 'ckpt_name'"), "got: {err}");
    }

    #[test]
    fn rejects_a_missing_workflow_and_a_bad_endpoint() {
        let d = tmpdir();
        let p = write(
            &d,
            "worker.toml",
            r#"
[worker]
[[backends]]
kind = "comfyui"
endpoint = "http://127.0.0.1:8188"
workflow = "nope.json"
checkpoint_id = "sdxl"
[backends.slots]
prompt = "6.inputs.text"
"#,
        );
        assert!(Config::load(&p)
            .unwrap_err()
            .to_string()
            .contains("workflow not found"));

        let p = write(
            &d,
            "bad.toml",
            r#"
[worker]
[[backends]]
kind = "vllm"
endpoint = "ftp://127.0.0.1:8000"
"#,
        );
        assert!(Config::load(&p)
            .unwrap_err()
            .to_string()
            .contains("http or https"));
    }

    #[test]
    fn rejects_a_worker_with_no_backends() {
        let d = tmpdir();
        let p = write(&d, "worker.toml", "[worker]\n");
        assert!(Config::load(&p)
            .unwrap_err()
            .to_string()
            .contains("no backends"));
    }
}

/// Getting paid.
///
/// A priced job is prepaid in 1M-token slices on submit so the worker can
/// stream. After the job, `job.pay` captures the actual bill; if it never
/// arrives the prepaid amount is settled. Off unless a contract address is
/// set — a node with nowhere to settle should serve rather than refuse.
#[derive(Clone, Deserialize, Serialize)]
pub struct PaymentsConfig {
    /// The pot contract. Empty means this node is not charging.
    #[serde(default)]
    pub contract: String,
    /// Base mainnet. Base Sepolia is 84532. Anvil is 31337.
    #[serde(default = "default_chain_id")]
    pub chain_id: u64,
    /// JSON-RPC URL for the settlement chain. Used to check the on-chain
    /// lock before work and to submit SpendTickets so collection does not
    /// depend on the client.
    #[serde(default)]
    pub rpc: String,
    /// Address settle is sent from. Derived from `key` when that is set.
    /// Empty, with no key, means tickets are kept on disk for a later collect.
    #[serde(default)]
    pub sender: String,
    /// Ethereum private key (secp256k1, 32-byte hex) this node signs settle
    /// with. Never printed. Prefer `ROOTMODE_PAY_KEY` in the environment.
    #[serde(default, skip_serializing)]
    pub key: String,
    /// If set, load (or generate) the pay key here. Each process needs its
    /// own file so concurrent settles do not share a nonce.
    #[serde(default)]
    pub key_file: PathBuf,
    /// Refuse jobs that arrive without a valid authorisation (legacy
    /// spend-on-submit) or, for priced jobs, without a payer address.
    #[serde(default)]
    pub require_auth: bool,
    /// Where the open channels are kept. Written as each authorisation
    /// arrives, because losing one means the jobs since the last were free.
    #[serde(default = "default_channels_file")]
    pub channels_file: PathBuf,
}

impl Default for PaymentsConfig {
    fn default() -> Self {
        Self {
            contract: String::new(),
            chain_id: default_chain_id(),
            rpc: String::new(),
            sender: String::new(),
            key: String::new(),
            key_file: PathBuf::new(),
            require_auth: false,
            channels_file: default_channels_file(),
        }
    }
}

impl std::fmt::Debug for PaymentsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentsConfig")
            .field("contract", &self.contract)
            .field("chain_id", &self.chain_id)
            .field("rpc", &self.rpc)
            .field("sender", &self.sender)
            .field("key", &if self.key.trim().is_empty() { "" } else { "***" })
            .field("key_file", &self.key_file)
            .field("require_auth", &self.require_auth)
            .field("channels_file", &self.channels_file)
            .finish()
    }
}

impl PaymentsConfig {
    /// Fill `key` from the environment or `key_file` (generated on first
    /// start), and derive `sender` from the key so settle has a from-address.
    pub fn apply_key(&mut self) {
        let k = self.key.trim();
        if k.is_empty() || k.starts_with("${") {
            self.key = std::env::var("ROOTMODE_PAY_KEY").unwrap_or_default();
        }
        if self.key.trim().is_empty() && !self.key_file.as_os_str().is_empty() {
            match load_or_create_pay_key(&self.key_file) {
                Ok(hex) => self.key = hex,
                Err(e) => tracing::warn!("payments.key_file: {e}"),
            }
        }
        let raw = self.key.trim().trim_start_matches("0x");
        if raw.is_empty() {
            return;
        }
        let Ok(bytes) = hex::decode(raw) else {
            return;
        };
        if bytes.len() != 32 {
            return;
        }
        let Ok(sk) = k256::ecdsa::SigningKey::from_bytes((&bytes[..]).into()) else {
            return;
        };
        let addr = rootmode_core::payments::address_of(sk.verifying_key());
        if self.sender.trim().is_empty() {
            self.sender = addr.clone();
        }
        tracing::info!("settle signer {addr}");
    }

    /// The domain these signatures must be good for, or `None` when this node
    /// is not charging.
    pub fn domain(&self) -> Option<rootmode_core::payments::Domain> {
        let contract = self.contract.trim();
        if contract.is_empty() {
            return None;
        }
        Some(rootmode_core::payments::Domain {
            chain_id: self.chain_id,
            verifying_contract: contract.to_string(),
        })
    }
}

/// The live deployment, as written by `contracts/deploy-base.sh`. The
/// desktop bundles the same file; a pot redeploy updates both in one commit.
const BUNDLED_CHAIN: &str = include_str!("../chain.base.json");

struct BundledChain {
    chain_id: u64,
    pot: String,
    rpc: String,
}

fn bundled_chain() -> Option<BundledChain> {
    let v: serde_json::Value = serde_json::from_str(BUNDLED_CHAIN).ok()?;
    let pot = v.get("pot")?.as_str()?.trim().to_string();
    if pot.is_empty() {
        return None;
    }
    Some(BundledChain {
        chain_id: v.get("chainId")?.as_u64()?,
        pot,
        rpc: v.get("rpc")?.as_str()?.trim().to_string(),
    })
}

/// The pot address built into this binary, for logs and docs.
pub fn bundled_pot() -> Option<String> {
    bundled_chain().map(|c| c.pot)
}

fn default_chain_id() -> u64 {
    8453
}

fn default_channels_file() -> PathBuf {
    PathBuf::from("channels.json")
}

fn load_or_create_pay_key(path: &Path) -> Result<String> {
    if path.exists() {
        let hex = std::fs::read_to_string(path).map_err(|e| {
            WorkerError::Config(format!("cannot read {}: {e}", path.display()))
        })?;
        return Ok(hex.trim().to_string());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            WorkerError::Config(format!("cannot create {}: {e}", dir.display()))
        })?;
    }
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let sk = k256::ecdsa::SigningKey::from_bytes((&bytes[..]).into())
        .map_err(|e| WorkerError::Config(format!("pay key: {e}")))?;
    let hex = hex::encode(sk.to_bytes());
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &hex)
        .map_err(|e| WorkerError::Config(format!("cannot write {}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| WorkerError::Config(format!("cannot write {}: {e}", path.display())))?;
    Ok(hex)
}
