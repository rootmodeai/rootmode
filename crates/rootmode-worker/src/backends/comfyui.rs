//! ComfyUI.
//!
//! The operator exports one workflow in API format. That graph is the only
//! thing this worker will run, and the client's entire contribution to it is
//! the prompt: it cannot send a graph, add a node, change a checkpoint path,
//! or alter how the render is done. A hostile prompt can therefore be nothing
//! worse than a bad picture.
//!
//! Steps, guidance, size and scheduler are whatever the operator saved in the
//! workflow. They are not client parameters because a client cannot know the
//! right values for a pipeline it has never seen. The worker reports what it
//! used in the result so the numbers are visible afterwards.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use rootmode_core::{
    sha256_hex, ImageParams, JobKind, JobPayload, JobResult, ModelDescriptor, Price,
    PROTOCOL_VERSION,
};
use serde_json::Value;
use uuid::Uuid;

use super::{uiformat, Backend, Progress};
use crate::config::ComfyConfig;
use crate::error::{Result, WorkerError};

pub struct ComfyBackend {
    config: ComfyConfig,
    /// What clients see this backend serving. The configured name when there
    /// is one, otherwise the checkpoint the server turned out to have — a
    /// model advertised under an empty name is one nobody can ask for.
    model_id: String,
    /// Every checkpoint installed on the box, as filenames, when the worker
    /// generated the graph itself. Re-read on each discovery, so a checkpoint
    /// dropped into `ComfyUI/models/checkpoints` while the worker is running
    /// becomes servable without a restart.
    ///
    /// Empty for an operator's own workflow: that graph names its checkpoint
    /// wherever it likes, and swapping it would be guessing at their pipeline.
    installed: std::sync::RwLock<Vec<String>>,
    template: Value,
    /// A graph per model, for the boxes where one shape does not fit
    /// everything. Checked before the built-in graph, so an operator who
    /// exported a pipeline for a checkpoint gets theirs rather than ours.
    per_model: Vec<LoadedWorkflow>,
    /// What each checkpoint turned out to need, learned from its first run.
    /// A box pays one failed attempt per checkpoint, once, and never again.
    shapes: std::sync::RwLock<std::collections::BTreeMap<String, Shape>>,
    /// And which encoder family each one wanted, once ComfyUI has said.
    clip_kinds: std::sync::RwLock<std::collections::BTreeMap<String, String>>,
    /// Models this node tried and could not render, with the reason.
    ///
    /// The point of keeping this is what it stops: a node in an open network
    /// advertising a model it cannot actually serve, and failing a stranger's
    /// job every time they pick it. One failure retires the advertisement.
    /// Cleared whenever the box's installed files change, because installing
    /// the missing encoder is exactly how an operator fixes it.
    unservable: std::sync::RwLock<std::collections::BTreeMap<String, String>>,
    /// What was installed last time we looked, to notice that.
    shelf: std::sync::RwLock<String>,
    /// The operator's own saved workflows, read from ComfyUI and converted to
    /// the form `/prompt` runs. Keyed by the model each one loads.
    ///
    /// This is the answer to "will it work on a stranger's box": whatever they
    /// built in their own editor and can render in their own browser, this
    /// serves — no export, no config, no guessing at graph shapes.
    saved: std::sync::RwLock<Vec<LoadedWorkflow>>,
    http: reqwest::Client,
}

/// One of the operator's `workflow_for` entries, read and validated at start.
struct LoadedWorkflow {
    model: String,
    template: Value,
    slots: std::collections::BTreeMap<String, String>,
}

/// Where the built-in graph puts the two things the worker fills.
fn default_slots() -> std::collections::BTreeMap<String, String> {
    [("prompt", "6.inputs.text"), ("seed", "3.inputs.seed")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The same, but starting from a picture the client sent.
///
/// Two nodes differ from [`default_graph`]: the latent comes from encoding an
/// uploaded image rather than from empty noise, and the sampler denoises only
/// partway, so what was there survives in proportion to how little we ask it
/// to change.
fn img2img_graph(checkpoint: &str, filename: &str, denoise: f32) -> Value {
    let mut graph = default_graph(checkpoint);
    graph["10"] = serde_json::json!({
        "class_type": "LoadImage",
        "inputs": { "image": filename }
    });
    graph["11"] = serde_json::json!({
        "class_type": "VAEEncode",
        "inputs": { "pixels": ["10", 0], "vae": ["4", 2] }
    });
    // The sampler reads that instead of the empty latent, which is now
    // unreferenced — ComfyUI only executes what the output depends on, so it
    // costs nothing to leave in place.
    graph["3"]["inputs"]["latent_image"] = serde_json::json!(["11", 0]);
    graph["3"]["inputs"]["denoise"] = serde_json::json!(denoise);
    graph
}

/// Repaint one region and leave the rest of the picture alone.
///
/// The difference from [`img2img_graph`] is `SetLatentNoiseMask`: the sampler
/// is handed the encoded picture *and* a mask saying which parts it may touch.
/// Everything outside the mask comes back as it went in, so the denoise can be
/// pushed hard — the whole point is a real change inside the mask — without
/// costing the face, the setting, or anything else the person wanted kept.
///
/// The mask is grown by a few pixels first. A mask traced exactly to an edge
/// leaves a seam, because the sampler needs a little context on both sides to
/// blend against.
fn inpaint_graph(checkpoint: &str, image: &str, mask: &str, denoise: f32) -> Value {
    let mut graph = img2img_graph(checkpoint, image, denoise);
    graph["12"] = serde_json::json!({
        "class_type": "LoadImageMask",
        // Red rather than alpha: the app sends an opaque black-and-white PNG,
        // and reading alpha from that would find every pixel equally opaque.
        "inputs": { "image": mask, "channel": "red" }
    });
    graph["13"] = serde_json::json!({
        "class_type": "GrowMask",
        "inputs": { "mask": ["12", 0], "expand": 12, "tapered_corners": true }
    });
    graph["14"] = serde_json::json!({
        "class_type": "SetLatentNoiseMask",
        "inputs": { "samples": ["11", 0], "mask": ["13", 0] }
    });
    graph["3"]["inputs"]["latent_image"] = serde_json::json!(["14", 0]);

    // Put the original pixels back everywhere the mask did not cover.
    //
    // Without this the untouched parts still drift slightly, because the whole
    // image is encoded to latents and decoded again and that round trip is
    // lossy. Small, but it is exactly the promise being made — "your face is
    // untouched" should mean untouched, not nearly — and it compounds over a
    // few edits.
    graph["15"] = serde_json::json!({
        "class_type": "ImageCompositeMasked",
        "inputs": {
            "destination": ["10", 0],  // what the client sent
            "source": ["8", 0],        // what came out of the sampler
            "x": 0, "y": 0, "resize_source": false,
            "mask": ["13", 0],
        }
    });
    graph["9"]["inputs"]["images"] = serde_json::json!(["15", 0]);
    graph
}

/// How hard to push the prompt, for a checkpoint we know nothing about but its
/// name.
///
/// Flux and SD3 are distilled: they expect a guidance of 1.0, and the value
/// that suits SDXL burns them to noise. Serving every checkpoint on the box
/// means the graph now meets models the SDXL defaults ruin, and a picture that
/// comes back scorched reads as "this network is broken" rather than "that
/// checkpoint wants different settings".
///
/// A filename is weak evidence, so this only moves the one setting that is
/// catastrophic to get wrong. An operator whose pipeline needs more than a
/// number nudged should export a workflow — that path is unchanged.
fn guidance_for(checkpoint: &str) -> f32 {
    let name = checkpoint.to_lowercase();
    let distilled = ["flux", "schnell", "sd3", "sd35", "sd3.5", "turbo", "lightning"];
    if distilled.iter().any(|hint| name.contains(hint)) {
        1.0
    } else {
        6.0
    }
}

/// What a checkpoint turned out to need.
///
/// Learned, not configured. A file in `models/checkpoints` may be an
/// all-in-one — model, text encoder and VAE in one safetensors — or it may be
/// only the diffusion weights, with the encoders installed separately. Nothing
/// ComfyUI exposes says which, so the worker finds out the only way there is:
/// it runs the ordinary graph, and if the encoder comes back empty it builds
/// the other shape and runs that instead. The answer is remembered per
/// checkpoint, so the cost is one attempt on a box's first job, ever.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    /// `CheckpointLoaderSimple` supplies model, clip and vae.
    Bundled,
    /// `UNETLoader` + `DualCLIPLoader`/`CLIPLoader` + `VAELoader`.
    Split,
}

/// The encoders and VAE a split graph can draw on, as ComfyUI reports them.
#[derive(Debug, Clone, Default)]
struct Parts {
    clips: Vec<String>,
    vaes: Vec<String>,
    unets: Vec<String>,
    /// Every family `CLIPLoader` knows how to load — `flux`, `qwen_image`,
    /// `krea2`, and whatever a newer ComfyUI has added. Read rather than
    /// hard-coded: the list grows with every release, and a worker that
    /// shipped its own copy would be wrong within a month.
    clip_types: Vec<String>,
}

impl Parts {
    /// The two text encoders a Flux-shaped model wants, in the order
    /// `DualCLIPLoader` expects them.
    ///
    /// Picked by name because that is the only signal there is: `clip_l` and
    /// a `t5` are what these models were trained against, and a box that has
    /// them almost always named them so.
    fn flux_pair(&self) -> Option<(String, String)> {
        let find = |needle: &str| {
            self.clips
                .iter()
                .find(|c| c.to_lowercase().contains(needle))
                .cloned()
        };
        match (find("t5"), find("clip_l").or_else(|| find("clip-l"))) {
            (Some(t5), Some(l)) => Some((t5, l)),
            _ => None,
        }
    }

    /// A single encoder, for models that use one.
    fn single_clip(&self) -> Option<String> {
        self.clips.first().cloned()
    }

    /// The encoder to load for a named family.
    ///
    /// Families are built on a particular text model — `krea2` and
    /// `qwen_image` on Qwen, `flux` and `sd3` on T5 — so a file whose name
    /// says which it is beats taking the first on the list. Where nothing
    /// says, the single encoder installed is the only candidate there is.
    fn clip_for(&self, kind: &str) -> Option<String> {
        const FAMILIES: [(&str, &str); 6] = [
            ("krea2", "qwen"),
            ("qwen_image", "qwen"),
            ("ovis", "qwen"),
            ("flux", "t5"),
            ("flux2", "t5"),
            ("sd3", "t5"),
        ];
        let wanted = FAMILIES
            .iter()
            .find(|(family, _)| *family == kind)
            .map(|(_, encoder)| *encoder);

        wanted
            .and_then(|needle| {
                self.clips
                    .iter()
                    .find(|c| c.to_lowercase().contains(needle))
                    .cloned()
            })
            .or_else(|| self.single_clip())
    }

    /// The VAE to decode with. `ae.safetensors` is the Flux one and is what
    /// these checkpoints are usually paired with; otherwise take what there is.
    fn vae(&self) -> Option<String> {
        self.vaes
            .iter()
            .find(|v| v.to_lowercase().starts_with("ae."))
            .or_else(|| self.vaes.iter().find(|v| *v != "pixel_space"))
            .cloned()
    }
}

/// The same picture, from a checkpoint that carries only diffusion weights.
///
/// `UNETLoader` for the model, a text encoder loaded separately, and a VAE to
/// decode with — the shape Flux, Krea and SD3-style releases need. Node ids
/// match [`default_graph`] wherever they can (6 is the prompt, 3 the sampler),
/// so everything downstream that fills slots keeps working.
fn split_graph(
    unet: &str,
    clip: &ClipChoice,
    vae: &str,
    guidance: f32,
    recipe: &uiformat::Recipe,
) -> Value {
    let clip_node = match clip {
        ClipChoice::Dual { t5, l, kind } => serde_json::json!({
            "class_type": "DualCLIPLoader",
            "inputs": { "clip_name1": t5, "clip_name2": l, "type": kind }
        }),
        ClipChoice::Single { name, kind } => serde_json::json!({
            "class_type": "CLIPLoader",
            "inputs": { "clip_name": name, "type": kind }
        }),
    };

    serde_json::json!({
        "4":  { "class_type": "UNETLoader",
                "inputs": { "unet_name": unet, "weight_dtype": "default" } },
        "11": clip_node,
        "10": { "class_type": "VAELoader", "inputs": { "vae_name": vae } },
        // The latent's class decides how many channels the noise has. A
        // sixteen-channel model handed four-channel noise renders something —
        // and that something looks like a photographic negative made of
        // porridge. Taken from the model's own template where there is one.
        "5":  { "class_type": recipe.latent,
                "inputs": { "width": 1024, "height": 1024, "batch_size": 1 } },
        "6":  { "class_type": "CLIPTextEncode", "inputs": { "text": "", "clip": ["11", 0] } },
        "7":  { "class_type": "CLIPTextEncode", "inputs": { "text": "", "clip": ["11", 0] } },
        "3":  { "class_type": "KSampler",
                "inputs": {
                    "seed": 0,
                    "steps": recipe.steps.unwrap_or(25),
                    "cfg": recipe.cfg.unwrap_or(guidance as f64),
                    "sampler_name": recipe.sampler.clone().unwrap_or_else(|| "euler".into()),
                    "scheduler": recipe.scheduler.clone().unwrap_or_else(|| "normal".into()),
                    "denoise": 1.0,
                    "model": ["4", 0], "positive": ["6", 0],
                    "negative": ["7", 0], "latent_image": ["5", 0] } },
        "8":  { "class_type": "VAEDecode", "inputs": { "samples": ["3", 0], "vae": ["10", 0] } },
        "9":  { "class_type": "SaveImage",
                "inputs": { "filename_prefix": "rootmode", "images": ["8", 0] } },
    })
}

/// Which encoder nodes a split graph will use.
#[derive(Debug, Clone, PartialEq)]
enum ClipChoice {
    Dual { t5: String, l: String, kind: String },
    Single { name: String, kind: String },
}


/// The encoder family this model wants, from the ones ComfyUI offers.
///
/// A checkpoint called `krea2TurboOfficialComfy…` is a `krea2`, and loading
/// its encoder as a plain `stable_diffusion` produces conditioning of the
/// wrong shape — the sampler then fails with a sentence about feature counts.
/// The name is the only signal available before running, so the longest type
/// that appears in it wins; short ones are ignored, or `ace` would match
/// `surface`.
fn clip_type_for(model: &str, types: &[String]) -> Option<String> {
    let flat: String = model
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    types
        .iter()
        .filter(|t| t.len() >= 4)
        .filter(|t| {
            let flat_type: String = t.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            flat.contains(&flat_type)
        })
        .max_by_key(|t| t.len())
        .cloned()
}

/// The encoder family ComfyUI itself asked for, when a run failed because the
/// wrong one was used.
///
/// The message says which — *"Load the text encoder with CLIPLoader type
/// 'krea2'"* — so the worker can take the instruction rather than making the
/// operator read it and edit a config.
fn suggested_clip_type(history: &Value) -> Option<String> {
    let said = execution_error(history)?;
    let (_, after) = said.split_once("type '")?;
    let (kind, _) = after.split_once('\'')?;
    (!kind.trim().is_empty()).then(|| kind.to_string())
}

/// Percent-encode a saved workflow's filename for the userdata path.
fn urlencode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Which installed file is the same model as `wanted`, if any.
///
/// A checkpoint and its diffusion-only twin are rarely named identically —
/// `krea2TurboOfficialComfy_krea2RawInt8Convrot` in `models/checkpoints` and
/// `lustify-v10-krea-turbo-int8_convrot` in `models/diffusion_models` are the
/// same weights — but they share the words that identify the model. So the
/// name is cut into words and the best overlap wins.
///
/// Words that appear in half the files on any box carry no information and
/// are ignored: matching on "safetensors" would pair anything with anything.
fn best_match(wanted: &str, candidates: &[String]) -> Option<String> {
    const NOISE: [&str; 12] = [
        "safetensors", "sft", "ckpt", "pt", "bin", "fp8", "fp16", "bf16",
        "official", "comfy", "model", "v",
    ];

    fn words(name: &str) -> Vec<String> {
        name.rsplit_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(name)
            .split(|c: char| !c.is_ascii_alphanumeric())
            .flat_map(|part| {
                // `krea2Turbo` is three words to a person and one to a
                // splitter: break on case changes and on the letter/digit
                // boundary, or `krea2` never matches `krea`.
                let mut out = Vec::new();
                let mut current = String::new();
                let mut last_was_digit = false;
                for ch in part.chars() {
                    let boundary = (ch.is_ascii_uppercase() && !current.is_empty())
                        || (ch.is_ascii_digit() != last_was_digit && !current.is_empty());
                    if boundary {
                        out.push(std::mem::take(&mut current));
                    }
                    last_was_digit = ch.is_ascii_digit();
                    current.push(ch.to_ascii_lowercase());
                }
                if !current.is_empty() {
                    out.push(current);
                }
                out
            })
            .filter(|w| w.len() > 1 && !NOISE.contains(&w.as_str()))
            .collect()
    }

    let target = words(wanted);
    if target.is_empty() {
        return None;
    }

    let mut best: Option<(usize, &String)> = None;
    for candidate in candidates {
        let theirs = words(candidate);
        let shared = target.iter().filter(|w| theirs.contains(w)).count();
        if shared >= 2 && best.is_none_or(|(score, _)| shared > score) {
            best = Some((shared, candidate));
        }
    }
    best.map(|(_, name)| name.clone())
}

/// Did this run fail because the checkpoint has no text encoder in it?
///
/// That is the signature of an all-in-one graph pointed at diffusion-only
/// weights, and the one failure worth rebuilding and retrying for.
fn needs_split(history: &Value) -> bool {
    execution_error(history).is_some_and(|e| {
        let e = e.to_lowercase();
        (e.contains("clip") && e.contains("invalid"))
            || e.contains("does not contain a valid clip")
            || (e.contains("vae") && e.contains("invalid"))
    })
}

/// A plain text-to-image graph, in the shape ComfyUI's own default workflow
/// has: load a checkpoint, encode a positive and a negative prompt, sample,
/// decode, save.
///
/// This exists so that pointing the worker at an endpoint is the whole
/// configuration. ComfyUI has no "generate an image" call — `/prompt` takes a
/// graph, and the graph *is* the program — so something has to supply one, and
/// making the operator hand-write it for the ordinary case is a poor trade.
///
/// It covers Stable Diffusion and SDXL checkpoints, which is most of them.
/// Anything with a different shape — Flux, SD3, video, LoRAs, ControlNet —
/// needs a real workflow exported from the web UI.
fn default_graph(checkpoint: &str) -> Value {
    serde_json::json!({
        "4": { "class_type": "CheckpointLoaderSimple",
               "inputs": { "ckpt_name": checkpoint } },
        "5": { "class_type": "EmptyLatentImage",
               "inputs": { "width": 1024, "height": 1024, "batch_size": 1 } },
        "6": { "class_type": "CLIPTextEncode",
               "inputs": { "text": "", "clip": ["4", 1] } },
        "7": { "class_type": "CLIPTextEncode",
               "inputs": { "text": "", "clip": ["4", 1] } },
        "3": { "class_type": "KSampler",
               "inputs": {
                   "seed": 0, "steps": 25, "cfg": guidance_for(checkpoint),
                   "sampler_name": "euler", "scheduler": "normal", "denoise": 1.0,
                   "model": ["4", 0], "positive": ["6", 0],
                   "negative": ["7", 0], "latent_image": ["5", 0] } },
        "8": { "class_type": "VAEDecode",
               "inputs": { "samples": ["3", 0], "vae": ["4", 2] } },
        "9": { "class_type": "SaveImage",
               "inputs": { "filename_prefix": "rootmode", "images": ["8", 0] } },
    })
}

impl ComfyBackend {
    /// Build the backend, asking the server what it can do when no workflow
    /// was configured.
    pub async fn new(config: ComfyConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| WorkerError::backend("comfyui", e))?;

        let mut discovered: Option<String> = None;
        let mut installed: Vec<String> = Vec::new();
        let (template, slots) = match &config.workflow {
            Some(path) => (load_workflow(path)?, config.slots.clone()),
            None => {
                // A server that is not up yet is not a reason to refuse to
                // start. ComfyUI is commonly launched beside the worker and
                // takes a minute to load; a node that exits here would be a
                // node an operator has to remember to start twice, in order.
                // The shelf fills in on the next refresh.
                installed = match list_checkpoints(&http, &config).await {
                    Ok(found) => found,
                    Err(e) => {
                        tracing::warn!(
                            "comfyui at {} is not answering ({e}); starting with nothing to \
                             offer and looking again every refresh",
                            config.endpoint
                        );
                        Vec::new()
                    }
                };
                let checkpoint = pick_checkpoint(&installed, &config.checkpoint_id);
                tracing::info!(
                    "no workflow configured; serving {} checkpoint(s) with the standard \
                     text-to-image graph, default {checkpoint}: {}",
                    installed.len(),
                    installed.join(", ")
                );
                discovered = Some(checkpoint.clone());
                (default_graph(&checkpoint), default_slots())
            }
        };

        let model_id = match (config.checkpoint_id.trim(), discovered) {
            ("", Some(found)) => tidy_model_name(&found),
            ("", None) => "image".to_string(),
            (named, _) => named.to_string(),
        };

        // Fail at startup, not on the first job: every declared slot must
        // actually exist in the graph.
        for (field, path) in &slots {
            let mut probe = template.clone();
            navigate(&mut probe, path).map_err(|_| {
                WorkerError::Config(format!(
                    "slot '{field}' points at '{path}', which does not exist in the workflow"
                ))
            })?;
        }
        // Each per-model graph, read and slot-checked now rather than on the
        // first job that asks for it: a typo should stop the node starting,
        // not fail a stranger's render at 3am.
        let mut per_model = Vec::new();
        for choice in &config.workflow_for {
            let template = load_workflow(&choice.file)?;
            let slots = if choice.slots.is_empty() {
                default_slots()
            } else {
                choice.slots.clone()
            };
            for (field, path) in &slots {
                let mut probe = template.clone();
                navigate(&mut probe, path).map_err(|_| {
                    WorkerError::Config(format!(
                        "workflow for '{}': slot '{field}' points at '{path}', which does not \
                         exist in {}",
                        choice.model,
                        choice.file.display()
                    ))
                })?;
            }
            tracing::info!(
                "serving {} with {}",
                choice.model,
                choice.file.display()
            );
            per_model.push(LoadedWorkflow {
                model: choice.model.trim().to_string(),
                template,
                slots,
            });
        }

        let mut config = config;
        config.slots = slots;

        Ok(Self {
            config,
            model_id,
            installed: std::sync::RwLock::new(installed),
            template,
            per_model,
            shapes: std::sync::RwLock::new(Default::default()),
            clip_kinds: std::sync::RwLock::new(Default::default()),
            unservable: std::sync::RwLock::new(Default::default()),
            shelf: std::sync::RwLock::new(String::new()),
            saved: std::sync::RwLock::new(Vec::new()),
            http,
        })
    }

    /// The checkpoint the generated graph loads, when this backend generated
    /// one. `None` for an operator's own workflow — we cannot know which node
    /// in a graph we have never seen loads the model.
    fn loaded_checkpoint(&self) -> Option<String> {
        if self.config.workflow.is_some() {
            return None;
        }
        self.template
            .pointer("/4/inputs/ckpt_name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// What ComfyUI has installed that a split graph could use.
    ///
    /// Read from `/object_info`, which lists the files each loader can offer —
    /// the same lists the web UI populates its dropdowns from.
    async fn parts(&self) -> Parts {
        let options = |info: &Value, node: &str, field: &str| -> Vec<String> {
            info.pointer(&format!("/{node}/input/required/{field}/0"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut parts = Parts::default();
        for (node, field, sink) in [
            ("DualCLIPLoader", "clip_name1", 0),
            ("VAELoader", "vae_name", 1),
            ("UNETLoader", "unet_name", 2),
            ("CLIPLoader", "type", 3),
        ] {
            let url = self.url(&format!("/object_info/{node}"));
            let Ok(resp) = self.http.get(&url).send().await else {
                continue;
            };
            let Ok(info) = resp.json::<Value>().await else {
                continue;
            };
            let names = options(&info, node, field);
            match sink {
                0 => parts.clips = names,
                1 => parts.vaes = names,
                2 => parts.unets = names,
                _ => parts.clip_types = names,
            }
        }
        parts
    }

    /// A split graph for `checkpoint`, or the reason one cannot be built.
    ///
    /// The failure matters as much as the success: "this checkpoint needs a
    /// separate text encoder and none is installed" is something an operator
    /// can act on in a minute. "No image" is not.
    async fn split_for(
        &self,
        checkpoint: &str,
        guidance: f32,
        force_type: Option<&str>,
    ) -> Result<Value> {
        let parts = self.parts().await;

        // The model file: whatever `UNETLoader` offers under this name, else
        // the file itself — a checkpoint directory entry usually also appears
        // as a diffusion model when it holds only weights.
        let unet = parts
            .unets
            .iter()
            .find(|u| u.eq_ignore_ascii_case(checkpoint))
            .cloned()
            .or_else(|| best_match(checkpoint, &parts.unets))
            // Sending a name ComfyUI does not have gets a validation error
            // about a value not in a list, which tells an operator nothing
            // about what actually went wrong. Say it plainly instead.
            .ok_or_else(|| {
                WorkerError::backend(
                    "comfyui",
                    format!(
                        "{} needs its diffusion weights loaded separately, and no matching file \
                         is in ComfyUI/models/diffusion_models. Installed there: {}.",
                        tidy_model_name(checkpoint),
                        if parts.unets.is_empty() {
                            "nothing".to_string()
                        } else {
                            parts.unets.join(", ")
                        }
                    ),
                )
            })?;

        // What this model calls for: what ComfyUI asked for after a failed
        // run, else what its name says, else the ordinary case.
        let kind = force_type
            .map(str::to_string)
            .or_else(|| clip_type_for(checkpoint, &parts.clip_types));

        let clip = match (&kind, parts.flux_pair()) {
            // A family was named, so one encoder loaded as that family — the
            // pair is only right for flux-shaped models.
            (Some(kind), _) if kind != "flux" => match parts.clip_for(kind) {
                Some(name) => ClipChoice::Single { name, kind: kind.clone() },
                None => {
                    return Err(WorkerError::backend(
                        "comfyui",
                        format!(
                            "{} wants a '{kind}' text encoder and none is installed. Put one in \
                             ComfyUI/models/text_encoders.",
                            tidy_model_name(checkpoint)
                        ),
                    ))
                }
            },
            (_, Some((t5, l))) => ClipChoice::Dual { t5, l, kind: "flux".into() },
            (_, None) => match parts.single_clip() {
                Some(name) => ClipChoice::Single {
                    name,
                    kind: kind.clone().unwrap_or_else(|| "stable_diffusion".into()),
                },
                None => {
                    return Err(WorkerError::backend(
                        "comfyui",
                        format!(
                            "{} carries no text encoder, and none is installed to pair with it. \
                             Put one in ComfyUI/models/text_encoders (Flux-style models want \
                             t5xxl and clip_l), or serve this model with a workflow of your own.",
                            tidy_model_name(checkpoint)
                        ),
                    ))
                }
            },
        };

        // What the model's own template says it needs.
        let family = match &clip {
            ClipChoice::Single { kind, .. } => Some(kind.clone()),
            ClipChoice::Dual { kind, .. } => Some(kind.clone()),
        };
        let recipe = self.recipe_for(checkpoint, family.as_deref()).await;

        // A named VAE that is not installed is the difference between a
        // correct picture and a plausible-looking wrong one, so it is an
        // error rather than a substitution.
        if let Some(wanted) = &recipe.vae {
            if !parts.vaes.iter().any(|v| v == wanted) {
                return Err(WorkerError::backend(
                    "comfyui",
                    format!(
                        "{} decodes with {wanted}, which is not installed — without it the \
                         picture comes back miscoloured rather than failing. Put it in \
                         ComfyUI/models/vae. Installed there: {}.",
                        tidy_model_name(checkpoint),
                        parts.vaes.join(", ")
                    ),
                ));
            }
        }

        let vae = recipe.vae.clone().or_else(|| parts.vae()).ok_or_else(|| {
            WorkerError::backend(
                "comfyui",
                format!(
                    "{} carries no VAE, and none is installed. Put one in ComfyUI/models/vae.",
                    tidy_model_name(checkpoint)
                ),
            )
        })?;

        // Only the pair is worth reporting: it is the choice an operator would
        // otherwise have made by hand in the web UI.
        tracing::info!(
            "{} needs a split graph; using {:?} + {vae}",
            tidy_model_name(checkpoint),
            clip
        );
        Ok(split_graph(&unet, &clip, &vae, guidance, &recipe))
    }

    /// Every recipe this ComfyUI ships, from the templates its node packs
    /// installed. Read rather than reasoned about: the pack's author knows
    /// how their model is meant to be run, and this worker does not.
    async fn recipes(&self) -> Vec<uiformat::Recipe> {
        let listing = match self
            .http
            .get(self.url("/api/workflow_templates"))
            .send()
            .await
        {
            Ok(r) => r
                .json::<std::collections::BTreeMap<String, Vec<String>>>()
                .await
                .unwrap_or_default(),
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        for (pack, names) in listing {
            for name in names {
                let path = format!("/api/workflow_templates/{pack}/{name}.json");
                let Ok(resp) = self.http.get(self.url(&path)).send().await else {
                    continue;
                };
                let Ok(ui) = resp.json::<Value>().await else { continue };
                out.push(uiformat::recipe_of(&ui));
            }
        }
        out
    }

    /// The recipe written for this model, if the box has one.
    ///
    /// Matched on the encoder family first — a template that loads its CLIP as
    /// `krea2` is about krea2 models whatever its file is called — then on the
    /// words the model names share.
    async fn recipe_for(&self, checkpoint: &str, family: Option<&str>) -> uiformat::Recipe {
        let recipes = self.recipes().await;
        if let Some(family) = family {
            if let Some(found) = recipes
                .iter()
                .find(|r| r.family.as_deref() == Some(family))
            {
                return found.clone();
            }
        }
        recipes
            .iter()
            .find(|r| {
                r.model
                    .as_deref()
                    .and_then(|m| best_match(checkpoint, &[m.to_string()]))
                    .is_some()
            })
            .cloned()
            .unwrap_or_default()
    }

    /// Read the workflows the operator saved in their own ComfyUI and convert
    /// each one. Failures are logged and skipped: a graph this worker cannot
    /// convert is one it must not offer, and the rest still stand.
    async fn read_saved_workflows(&self) -> Vec<LoadedWorkflow> {
        let listing = match self
            .http
            .get(self.url("/api/userdata?dir=workflows"))
            .send()
            .await
        {
            Ok(r) => r.json::<Vec<String>>().await.unwrap_or_default(),
            // An older ComfyUI without the endpoint: nothing to read, and the
            // built-in graphs carry on as before.
            Err(_) => return Vec::new(),
        };

        let catalogue = match self.http.get(self.url("/object_info")).send().await {
            Ok(r) => r.json::<Value>().await.unwrap_or(Value::Null),
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        for name in listing {
            let path = format!("/api/userdata/workflows%2F{}", urlencode(&name));
            let Ok(resp) = self.http.get(self.url(&path)).send().await else {
                continue;
            };
            let Ok(ui) = resp.json::<Value>().await else { continue };

            match uiformat::convert(&ui, &catalogue) {
                Ok(converted) => {
                    let Some(model) = converted.model else {
                        tracing::debug!("saved workflow {name} names no model; skipping");
                        continue;
                    };
                    tracing::info!(
                        "serving {} with your saved workflow {name}",
                        tidy_model_name(&model)
                    );
                    out.push(LoadedWorkflow {
                        model: tidy_model_name(&model),
                        template: converted.graph,
                        slots: converted.slots,
                    });
                }
                Err(why) => tracing::debug!("cannot use saved workflow {name}: {why}"),
            }
        }
        out
    }

    /// Stop offering a model this node has proven it cannot render.
    fn retire(&self, model: &str, why: &str) {
        let tidied = tidy_model_name(model);
        tracing::warn!("no longer advertising {tidied}: {why}");
        self.unservable
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tidied, why.to_string());
    }

    /// Forget every retirement when the box's files change: an operator who
    /// just installed the missing encoder should not have to restart to be
    /// asked again.
    fn refresh_shelf(&self, parts: &Parts) {
        let fingerprint = format!(
            "{}|{}|{}",
            parts.clips.join(","),
            parts.vaes.join(","),
            parts.unets.join(",")
        );
        let mut shelf = self.shelf.write().unwrap_or_else(|e| e.into_inner());
        if *shelf != fingerprint {
            if !shelf.is_empty() {
                tracing::info!("installed files changed; re-testing models that failed before");
                self.unservable
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
            }
            *shelf = fingerprint;
        }
    }

    fn remembered_clip(&self, checkpoint: &str) -> Option<String> {
        self.clip_kinds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(checkpoint)
            .cloned()
    }

    fn remember_clip(&self, checkpoint: &str, kind: &str) {
        self.clip_kinds
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(checkpoint.to_string(), kind.to_string());
    }

    fn remembered_shape(&self, checkpoint: &str) -> Option<Shape> {
        self.shapes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(checkpoint)
            .copied()
    }

    fn remember_shape(&self, checkpoint: &str, shape: Shape) {
        self.shapes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(checkpoint.to_string(), shape);
    }

    /// The operator's own saved graph for this model, if they have one.
    fn saved_for(&self, params: &ImageParams) -> Option<LoadedWorkflow> {
        let asked = params.checkpoint_id.as_deref().map(str::trim)?.to_lowercase();
        if asked.is_empty() {
            return None;
        }
        let saved = self.saved.read().unwrap_or_else(|e| e.into_inner());
        saved
            .iter()
            .find(|w| w.model.to_lowercase() == asked)
            .or_else(|| saved.iter().find(|w| w.model.to_lowercase().starts_with(&asked)))
            .map(|w| LoadedWorkflow {
                model: w.model.clone(),
                template: w.template.clone(),
                slots: w.slots.clone(),
            })
    }

    /// The operator's graph for this model, if they exported one.
    fn workflow_for(&self, params: &ImageParams) -> Option<&LoadedWorkflow> {
        let asked = params.checkpoint_id.as_deref().map(str::trim).unwrap_or("");
        if asked.is_empty() {
            // Nothing named: the first per-model graph is the default, which
            // is what an operator who listed one and nothing else expects.
            return self.per_model.first();
        }
        let asked = asked.to_lowercase();
        self.per_model
            .iter()
            .find(|w| w.model.to_lowercase() == asked)
            .or_else(|| {
                self.per_model
                    .iter()
                    .find(|w| w.model.to_lowercase().starts_with(&asked))
            })
    }

    /// Which checkpoint this job runs against.
    ///
    /// A job that names one gets it, if the box has it. A job that names
    /// nothing gets the graph's default. Only meaningful for the built-in
    /// graph: an operator's workflow loads what it loads.
    fn checkpoint_for(&self, params: &ImageParams) -> Result<Option<String>> {
        let Some(default) = self.loaded_checkpoint() else {
            return Ok(None);
        };
        let Some(asked) = params.checkpoint_id.as_deref().map(str::trim) else {
            return Ok(Some(default));
        };
        if asked.is_empty() {
            return Ok(Some(default));
        }

        let available = self.installed.read().unwrap_or_else(|e| e.into_inner());
        match match_checkpoint(&available, asked) {
            Some(found) => Ok(Some(found)),
            // Refuse rather than quietly rendering with different weights:
            // the picture would come back looking nothing like what was asked
            // for, with no way to tell why.
            None => Err(WorkerError::Rejected(format!(
                "checkpoint '{asked}' is not installed here (have: {})",
                available
                    .iter()
                    .map(|f| tidy_model_name(f))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Send a picture to ComfyUI and return the filename it stored it under.
    async fn upload_image(&self, encoded: &str, job_id: Uuid, role: &str) -> Result<String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| WorkerError::Rejected(format!("from_image is not valid base64: {e}")))?;

        // Named after the job so two clients starting from different pictures
        // at the same time cannot overwrite each other's input.
        let name = format!("rootmode-{}-{role}.png", job_id.simple());
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.clone())
            .mime_str("image/png")
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        let form = reqwest::multipart::Form::new()
            .part("image", part)
            .text("overwrite", "true");

        let resp = self
            .http
            .post(self.url("/upload/image"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(WorkerError::backend(
                "comfyui",
                format!(
                    "could not upload the starting picture: HTTP {status}: {}",
                    body.chars().take(200).collect::<String>()
                ),
            ));
        }

        // The server may rename it; use what it says it stored.
        let stored: Value = resp.json().await.map_err(|e| {
            WorkerError::backend("comfyui", format!("bad /upload/image reply: {e}"))
        })?;
        Ok(stored
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&name)
            .to_string())
    }

    fn url_of(endpoint: &str, path: &str) -> String {
        format!(
            "{}/{}",
            endpoint.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn ws_url(&self, client_id: &str) -> String {
        let base = self.config.endpoint.trim_end_matches('/');
        let base = base
            .strip_prefix("https://")
            .map(|rest| format!("wss://{rest}"))
            .or_else(|| {
                base.strip_prefix("http://")
                    .map(|rest| format!("ws://{rest}"))
            })
            .unwrap_or_else(|| base.to_string());
        format!("{base}/ws?clientId={client_id}")
    }

    /// Build the graph for one job. Pure — the interesting part to test.
    pub fn graph_for(&self, params: &ImageParams, seed: u64) -> Result<Value> {
        // A graph the operator built for this model runs as they built it:
        // its own nodes, its own loaders, its own slots. Only the prompt and
        // the seed are filled, exactly as with the single-workflow case.
        // A graph the operator saved for this model in their own editor.
        // Cloned out because the lock cannot be held across the borrow.
        if let Some(saved) = self.saved_for(params) {
            let mut graph = saved.template.clone();
            for (field, path) in &saved.slots {
                let value = match field.as_str() {
                    "prompt" => Value::String(params.prompt.clone()),
                    "seed" => Value::from(seed),
                    other => return Err(WorkerError::Config(format!("unknown slot '{other}'"))),
                };
                *navigate(&mut graph, path)? = value;
            }
            return Ok(graph);
        }

        if let Some(chosen) = self.workflow_for(params) {
            let mut graph = chosen.template.clone();
            for (field, path) in &chosen.slots {
                let value = match field.as_str() {
                    "prompt" => Value::String(params.prompt.clone()),
                    "seed" => Value::from(seed),
                    other => return Err(WorkerError::Config(format!("unknown slot '{other}'"))),
                };
                *navigate(&mut graph, path)? = value;
            }
            return Ok(graph);
        }

        let mut graph = self.template.clone();
        for (field, path) in &self.config.slots {
            let value = match field.as_str() {
                // The client's whole contribution to the graph.
                "prompt" => Value::String(params.prompt.clone()),
                // The worker's, so repeated prompts do not return the same
                // picture. Never client-supplied.
                "seed" => Value::from(seed),
                // Unreachable: config validation rejects unknown slots at load.
                other => {
                    return Err(WorkerError::Config(format!("unknown slot '{other}'")));
                }
            };
            let slot = navigate(&mut graph, path)?;
            *slot = value;
        }
        // Swap in the requested checkpoint. Only the built-in graph has a
        // known place to put it, and `checkpoint_for` returns None otherwise.
        if let Some(checkpoint) = self.checkpoint_for(params)? {
            *navigate(&mut graph, "4.inputs.ckpt_name")? = Value::String(checkpoint);
        }
        Ok(graph)
    }
}

/// How much noise to add back, from how much change was asked for.
///
/// `denoise` is the sampler's word for it and runs the same direction: 1.0
/// ignores the starting picture entirely. The default sits low enough that a
/// scene survives and high enough that the prompt can still add something.
fn change_to_denoise(change: Option<f32>) -> f32 {
    change.unwrap_or(0.45).clamp(0.05, 1.0)
}

/// A checkpoint filename, as a model name someone would type.
///
/// `lustifyNSFWCheckpoint_ggwpV7.safetensors` is a filename; `lustifyNSFWCheckpoint_ggwpV7`
/// is a name. Only the extension goes — anything cleverer would guess wrong at
/// what part of the name the operator considers meaningful.
fn tidy_model_name(filename: &str) -> String {
    filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .trim_end_matches(".safetensors")
        .trim_end_matches(".ckpt")
        .trim_end_matches(".sft")
        .to_string()
}

/// Per-image price for one advertised model.
///
/// Exact match first (case-insensitive, filename or tidied name), then a
/// prefix so `krea2` prices `krea2-turbo`. Anything unlisted uses `default`.
fn lookup_price(
    model_id: &str,
    default: Option<f64>,
    prices: &std::collections::BTreeMap<String, f64>,
) -> Option<f64> {
    let wanted = model_id.to_lowercase();
    let tidy_wanted = tidy_model_name(model_id).to_lowercase();
    prices
        .iter()
        .find(|(key, _)| {
            let key = key.to_lowercase();
            let tidy_key = tidy_model_name(&key).to_lowercase();
            key == wanted || tidy_key == wanted || tidy_key == tidy_wanted
        })
        .or_else(|| {
            prices.iter().find(|(key, _)| {
                let key = tidy_model_name(key).to_lowercase();
                !key.is_empty() && (wanted.starts_with(&key) || tidy_wanted.starts_with(&key))
            })
        })
        .map(|(_, amount)| *amount)
        .or(default)
}

/// Read a workflow the operator exported.
fn load_workflow(path: &std::path::Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        WorkerError::Config(format!("cannot read workflow {}: {e}", path.display()))
    })?;
    let template: Value = serde_json::from_str(&raw).map_err(|e| {
        WorkerError::Config(format!(
            "workflow {} is not valid JSON: {e}",
            path.display()
        ))
    })?;
    if !template.is_object() {
        return Err(WorkerError::Config(
            "workflow must be a ComfyUI API-format object (use 'Save (API Format)')".into(),
        ));
    }
    Ok(template)
}

/// Which checkpoint the generated graph should load.
///
/// The configured `checkpoint_id` is a label for clients, not a filename, so
/// it is matched loosely against what the server actually has — and when it
/// matches nothing, the server's own list is the answer rather than a guess.
/// Every checkpoint the server has, in the order it reports them.
async fn list_checkpoints(http: &reqwest::Client, config: &ComfyConfig) -> Result<Vec<String>> {
    let url = ComfyBackend::url_of(&config.endpoint, "/object_info/CheckpointLoaderSimple");
    let info: Value = http
        .get(&url)
        .send()
        .await
        .map_err(|e| {
            WorkerError::backend(
                "comfyui",
                format!(
                    "cannot ask {} what checkpoints it has ({e}). Start ComfyUI, or name a \
                     workflow so the worker does not need to.",
                    config.endpoint
                ),
            )
        })?
        .json()
        .await
        .map_err(|e| WorkerError::backend("comfyui", format!("bad /object_info response: {e}")))?;

    let available: Vec<String> = info
        .pointer("/CheckpointLoaderSimple/input/required/ckpt_name/0")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if available.is_empty() {
        return Err(WorkerError::Config(format!(
            "{} has no checkpoints installed — put one in ComfyUI/models/checkpoints",
            config.endpoint
        )));
    }

    Ok(available)
}

/// Which of the installed checkpoints a name refers to.
///
/// Clients see tidied names (`sdxl-base-1.0`), the server wants filenames
/// (`sdxl-base-1.0.safetensors`), and operators type whichever they remember,
/// so all three have to resolve. An empty name means "whatever is first".
fn match_checkpoint(available: &[String], wanted: &str) -> Option<String> {
    let wanted = wanted.trim().to_lowercase();
    if wanted.is_empty() {
        return available.first().cloned();
    }
    available
        .iter()
        .find(|name| name.to_lowercase() == wanted)
        .or_else(|| {
            available
                .iter()
                .find(|name| tidy_model_name(name).to_lowercase() == wanted)
        })
        .or_else(|| {
            available
                .iter()
                .find(|name| name.to_lowercase().starts_with(&wanted))
        })
        .cloned()
}

/// The startup default: what the operator named, else the first installed.
fn pick_checkpoint(available: &[String], configured: &str) -> String {
    // Nothing installed, or nothing answering yet. A placeholder rather than a
    // panic: the graph built from it is never run, because a backend with no
    // models advertises none and is never picked.
    if available.is_empty() {
        return if configured.trim().is_empty() {
            "image".to_string()
        } else {
            configured.to_string()
        };
    }
    match match_checkpoint(available, configured) {
        Some(name) => name,
        None => {
            let first = available[0].clone();
            tracing::info!(
                "no checkpoint matches '{configured}'; defaulting to {first}. Installed: {}",
                available.join(", ")
            );
            first
        }
    }
}

/// Walk `6.inputs.text` to a mutable slot. Every segment must already exist —
/// we fill declared inputs, we never invent nodes.
fn navigate<'a>(root: &'a mut Value, path: &str) -> Result<&'a mut Value> {
    let mut cursor = root;
    for segment in path.split('.') {
        cursor = cursor.get_mut(segment).ok_or_else(|| {
            WorkerError::Config(format!("workflow has no '{segment}' in '{path}'"))
        })?;
    }
    Ok(cursor)
}

#[derive(serde::Deserialize)]
struct QueueResponse {
    prompt_id: String,
}

impl ComfyBackend {
    fn advertised_price(&self, model_id: &str) -> Option<Price> {
        lookup_price(model_id, self.config.price, &self.config.prices).map(|amount| {
            Price {
                amount,
                currency: self.config.currency.clone(),
                ..Price::default()
            }
            .round_protocol()
        })
    }
}

#[async_trait]
impl Backend for ComfyBackend {
    fn name(&self) -> &str {
        "comfyui"
    }

    fn kind(&self) -> JobKind {
        JobKind::Image
    }

    async fn discover_models(&self) -> Result<Vec<ModelDescriptor>> {
        // Models the operator wired to a graph of their own. Exact, because
        // the pipeline exists: whatever shape the checkpoint has, there is
        // something here that can run it.
        let mut out: Vec<ModelDescriptor> = self
            .per_model
            .iter()
            .map(|w| ModelDescriptor {
                id: w.model.clone(),
                sha256: None,
                kind: JobKind::Image,
                price: self.advertised_price(&w.model),
                video: None,
            })
            .collect();

        // A single `workflow`: one advertised model. Listing every checkpoint
        // on the box would advertise capacity that graph cannot use — it
        // loads whichever one it names.
        if self.config.workflow.is_some() {
            out.push(ModelDescriptor {
                id: self.model_id.clone(),
                sha256: self.config.model_hash.clone(),
                kind: JobKind::Image,
                price: self.advertised_price(&self.model_id),
                video: None,
            });
            return Ok(out);
        }

        // Otherwise the built-in graph is in play, and it loads whatever it is
        // told to — so every installed checkpoint is servable. Re-read the
        // list: checkpoints get added to a box that is already running.
        let available = list_checkpoints(&self.http, &self.config).await?;
        *self.installed.write().unwrap_or_else(|e| e.into_inner()) = available.clone();
        let default = pick_checkpoint(&available, &self.config.checkpoint_id);

        // Notice new encoders or VAEs, which may make a retired model
        // servable again.
        self.refresh_shelf(&self.parts().await);

        // And re-read the operator's own workflows: they add one in the
        // editor, it is served here, with nothing else to do.
        let saved = self.read_saved_workflows().await;
        for w in &saved {
            out.push(ModelDescriptor {
                id: w.model.clone(),
                sha256: None,
                kind: JobKind::Image,
                price: self.advertised_price(&w.model),
                video: None,
            });
        }
        *self.saved.write().unwrap_or_else(|e| e.into_inner()) = saved;
        let from_saved: Vec<String> = self
            .saved
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|w| w.model.clone())
            .collect();
        let retired = self.unservable.read().unwrap_or_else(|e| e.into_inner()).clone();

        for filename in self.list_video_models().await {
            let id = tidy_model_name(&filename);
            if out.iter().any(|m| m.id.eq_ignore_ascii_case(&id)) {
                continue;
            }
            let price = self.advertised_price(&id);
            out.push(ModelDescriptor {
                id,
                sha256: None,
                kind: JobKind::Video,
                price,
                video: None,
            });
        }

        for filename in available {
            let id = tidy_model_name(&filename);
            // A checkpoint an operator already gave a graph to is theirs, not
            // ours: advertising it twice would let a client pick the built-in
            // graph for a model that needs the exported one.
            if self.per_model.iter().any(|w| w.model.eq_ignore_ascii_case(&id))
                || from_saved.iter().any(|m| m.eq_ignore_ascii_case(&id))
            {
                continue;
            }
            // Proven unservable on this box. Advertising it anyway is how an
            // open network fills up with peers that fail every job they are
            // sent — a claim nobody can check is worse than a shorter list.
            if let Some(why) = retired.get(&id) {
                tracing::debug!("not advertising {id}: {why}");
                continue;
            }
            let price = self.advertised_price(&id);
            out.push(ModelDescriptor {
                // Only the operator's own default can carry the hash they
                // attested to; the rest are whatever turned up on disk.
                sha256: if filename == default {
                    self.config.model_hash.clone()
                } else {
                    None
                },
                id,
                kind: JobKind::Image,
                price,
                video: None,
            });
        }
        Ok(out)
    }

    async fn health(&self) -> Result<String> {
        let resp = self
            .http
            .get(self.url("/system_stats"))
            .send()
            .await
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        if !resp.status().is_success() {
            return Err(WorkerError::backend(
                "comfyui",
                format!("HTTP {}", resp.status()),
            ));
        }
        Ok(match &self.config.workflow {
            Some(path) => format!("{} — workflow {}", self.config.endpoint, path.display()),
            None => format!("{} — standard text-to-image graph", self.config.endpoint),
        })
    }

    async fn run(
        &self,
        job_id: Uuid,
        payload: &JobPayload,
        progress: &Progress,
    ) -> Result<JobResult> {
        if let JobPayload::Video(params) = payload {
            return self.run_video(job_id, params, progress).await;
        }
        let JobPayload::Image(params) = payload else {
            return Err(WorkerError::Rejected(
                "comfyui backend only runs image and video jobs".into(),
            ));
        };

        // Derived from the job id: two prompts never collide, the same job
        // retried renders the same picture, and the seed used is reported back
        // so an operator can reproduce a result deliberately.
        let seed = u64::from_be_bytes(job_id.as_bytes()[..8].try_into().expect("uuid is 16 bytes"));
        // A starting picture has to reach ComfyUI before the graph can name
        // it: the graph refers to an image by filename, not by content.
        let graph = match &params.from_image {
            None => self.graph_for(params, seed)?,
            Some(encoded) => {
                let filename = self.upload_image(encoded, job_id, "from").await?;
                let denoise = match (&params.mask, params.change) {
                    // Inside a mask the rest of the picture is safe, so the
                    // default can be decisive rather than cautious: a timid
                    // repaint is the failure people actually hit.
                    (Some(_), None) => 0.9,
                    (_, change) => change_to_denoise(change),
                };
                let checkpoint = self.checkpoint_for(params)?.ok_or_else(|| {
                    WorkerError::Rejected(
                        "this worker runs a workflow of its own, which has no defined place to \
                         start from a picture. Ask its operator for an image-to-image workflow."
                            .into(),
                    )
                })?;
                let mut graph = match &params.mask {
                    None => img2img_graph(&checkpoint, &filename, denoise),
                    Some(mask) => {
                        let mask_file = self.upload_image(mask, job_id, "mask").await?;
                        inpaint_graph(&checkpoint, &filename, &mask_file, denoise)
                    }
                };
                // The prompt still goes where the standard graph puts it.
                *navigate(&mut graph, "6.inputs.text")? = Value::String(params.prompt.clone());
                *navigate(&mut graph, "3.inputs.seed")? = Value::from(seed);
                graph
            }
        };
        let history = self.render_with_shape(&graph, params, job_id, progress).await?;

        let (node, image) = first_image(&history).ok_or_else(|| {
            // ComfyUI knows exactly what went wrong and puts it in the
            // history entry. Reporting "no image" instead sends the operator
            // to the wrong place entirely — the graph did not quietly produce
            // nothing, a node raised.
            WorkerError::backend("comfyui", execution_error(&history).unwrap_or_else(||
                "the workflow finished without producing an image".to_string()))
        })?;

        let bytes = self.fetch_image(&image).await?;
        progress.set(0.99);

        Ok(JobResult {
            v: PROTOCOL_VERSION,
            job_id,
            kind: JobKind::Image,
            sha256: sha256_hex(&bytes),
            text: None,
            tool_calls: Vec::new(),
            image_path_or_b64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            thinking: None,
            meta: serde_json::json!({
                // What actually ran, which is not the default when the job
                // asked for one of the other installed checkpoints.
                "model": self
                    .checkpoint_for(params)
                    .ok()
                    .flatten()
                    .map(|c| tidy_model_name(&c))
                    .unwrap_or_else(|| self.model_id.clone()),
                "backend": "comfyui",
                "seed": seed,
                "node": node,
                "filename": image.filename,
            }),
        })
    }
}

impl ComfyBackend {
    /// Queue one graph and wait for it to finish, reporting progress.
    ///
    /// Returns the history entry, whatever it says — a failed run is a
    /// result here, because the caller may want to read the error and try a
    /// different shape rather than give up.
    async fn render(&self, graph: &Value, job_id: Uuid, progress: &Progress) -> Result<Value> {
        let client_id = job_id.to_string();

        // Subscribe before queueing so no progress is missed. Best effort:
        // without it the job still runs, just without a progress bar.
        let mut socket = match tokio_tungstenite::connect_async(self.ws_url(&client_id)).await {
            Ok((ws, _)) => Some(ws),
            Err(e) => {
                tracing::debug!("comfyui progress socket unavailable ({e}); polling only");
                None
            }
        };

        let resp = self
            .http
            .post(self.url("/prompt"))
            .json(&serde_json::json!({ "prompt": graph, "client_id": client_id }))
            .send()
            .await
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(400).collect();
            return Err(WorkerError::backend(
                "comfyui",
                format!("HTTP {status}: {snippet}"),
            ));
        }
        let queued: QueueResponse = resp
            .json()
            .await
            .map_err(|e| WorkerError::backend("comfyui", format!("bad /prompt response: {e}")))?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(self.config.timeout_secs);
        let history = loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(WorkerError::backend(
                    "comfyui",
                    "timed out waiting for the render",
                ));
            }

            // Watch the progress socket and poll history together: the socket
            // may be absent, and history is the authority on completion.
            let tick = tokio::time::sleep(Duration::from_millis(750));
            tokio::pin!(tick);

            tokio::select! {
                frame = next_text(&mut socket) => {
                    if let Some(text) = frame {
                        if let Some(fraction) = parse_progress(&text) {
                            progress.set(fraction);
                        }
                    }
                }
                _ = &mut tick => {}
            }

            if let Some(entry) = self.fetch_history(&queued.prompt_id).await? {
                break entry;
            }
        };

        Ok(history)
    }


    /// Render `graph`, and if ComfyUI says the checkpoint has no text encoder,
    /// build the other shape and run that instead.
    ///
    /// This is the whole of the "just give it your ComfyUI URL" promise: an
    /// operator should not have to know whether a file in `models/checkpoints`
    /// is an all-in-one or diffusion weights on their own, nor export a graph
    /// per model to find out. The worker tries, reads the error, and rebuilds.
    /// What it learns is remembered, so a checkpoint costs one wasted attempt
    /// once — not once per job.
    async fn render_with_shape(
        &self,
        graph: &Value,
        params: &ImageParams,
        job_id: Uuid,
        progress: &Progress,
    ) -> Result<Value> {
        // Only the built-in graph is ours to rebuild. An operator's own
        // workflow is theirs, and second-guessing it would be worse than the
        // error they get to read.
        let Some(checkpoint) = self.checkpoint_for(params)? else {
            return self.render(graph, job_id, progress).await;
        };
        if self.workflow_for(params).is_some() {
            return self.render(graph, job_id, progress).await;
        }

        let guidance = guidance_for(&checkpoint);
        if self.remembered_shape(&checkpoint) == Some(Shape::Split) {
            // Known to need it: go straight there rather than failing first.
            let split = self
                .split_for(&checkpoint, guidance, self.remembered_clip(&checkpoint).as_deref())
                .await?;
            let split = self.fill_standard_slots(split, params, job_id)?;
            return self.render(&split, job_id, progress).await;
        }

        let history = self.render(graph, job_id, progress).await?;
        if !needs_split(&history) {
            self.remember_shape(&checkpoint, Shape::Bundled);
            return Ok(history);
        }

        tracing::info!(
            "{} has no text encoder of its own; building a split graph and retrying",
            tidy_model_name(&checkpoint)
        );
        let split = self.split_for(&checkpoint, guidance, None).await?;
        let split = self.fill_standard_slots(split, params, job_id)?;
        let second = self.render(&split, job_id, progress).await?;

        // ComfyUI may answer with the family it actually wants — "load the
        // text encoder with CLIPLoader type 'krea2'". Taking that instruction
        // is the difference between a picture and an operator reading a
        // stack trace to edit a config by hand.
        if let Some(kind) = suggested_clip_type(&second) {
            tracing::info!(
                "{} wants a '{kind}' text encoder; loading it that way and retrying",
                tidy_model_name(&checkpoint)
            );
            let corrected = self.split_for(&checkpoint, guidance, Some(&kind)).await?;
            let corrected = self.fill_standard_slots(corrected, params, job_id)?;
            let third = self.render(&corrected, job_id, progress).await?;
            match execution_error(&third) {
                None => {
                    self.remember_shape(&checkpoint, Shape::Split);
                    self.remember_clip(&checkpoint, &kind);
                }
                // Told what it wanted, given exactly that, and still refused:
                // this node cannot serve this model as things stand.
                Some(why) => self.retire(&checkpoint, &why),
            }
            return Ok(third);
        }

        match execution_error(&second) {
            None => self.remember_shape(&checkpoint, Shape::Split),
            Some(why) if !needs_split(&second) => self.retire(&checkpoint, &why),
            Some(_) => {}
        }
        Ok(second)
    }

    /// Put the prompt and the seed where the worker's own graphs keep them.
    fn fill_standard_slots(&self, mut graph: Value, params: &ImageParams, job_id: Uuid) -> Result<Value> {
        let seed = u64::from_be_bytes(job_id.as_bytes()[..8].try_into().expect("uuid is 16 bytes"));
        *navigate(&mut graph, "6.inputs.text")? = Value::String(params.prompt.clone());
        *navigate(&mut graph, "3.inputs.seed")? = Value::from(seed);
        Ok(graph)
    }

    async fn fetch_history(&self, prompt_id: &str) -> Result<Option<Value>> {
        let resp = self
            .http
            .get(self.url(&format!("/history/{prompt_id}")))
            .send()
            .await
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| WorkerError::backend("comfyui", format!("bad /history response: {e}")))?;

        // ComfyUI answers `{}` until the prompt completes.
        match body.get(prompt_id) {
            Some(entry) if entry.get("outputs").is_some() => Ok(Some(entry.clone())),
            _ => Ok(None),
        }
    }

    async fn fetch_image(&self, image: &ImageRef) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(self.url("/view"))
            .query(&[
                ("filename", image.filename.as_str()),
                ("subfolder", image.subfolder.as_str()),
                ("type", image.folder_type.as_str()),
            ])
            .send()
            .await
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        if !resp.status().is_success() {
            return Err(WorkerError::backend(
                "comfyui",
                format!("fetching {}: HTTP {}", image.filename, resp.status()),
            ));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        if bytes.is_empty() {
            return Err(WorkerError::backend(
                "comfyui",
                "the rendered file was empty",
            ));
        }
        Ok(bytes.to_vec())
    }

    async fn list_video_models(&self) -> Vec<String> {
        let info = match object_info(&self.http, &self.config).await {
            Ok(info) => info,
            Err(_) => return Vec::new(),
        };
        // Only advertise a clip we can actually run. A Wan or LTX file on
        // disk is not a MiniMax graph, and listing it would fill the client's
        // video tab with a model that then fails every job.
        if !node_exists(&info, "MiniMaxH3ImageToVideo") {
            return Vec::new();
        }
        if !combo(&info, "CLIPLoader", "clip_name")
            .iter()
            .any(|n| is_minimax_clip(n))
        {
            return Vec::new();
        }
        if !combo(&info, "VAELoader", "vae_name")
            .iter()
            .any(|n| is_minimax_video_vae(n))
        {
            return Vec::new();
        }
        let mut names = combo(&info, "UNETLoader", "unet_name");
        names.extend(combo(&info, "UnetLoaderGGUF", "unet_name"));
        names
            .into_iter()
            .filter(|name| is_minimax_t2v(name))
            .collect()
    }

    async fn run_video(
        &self,
        job_id: Uuid,
        params: &rootmode_core::VideoParams,
        progress: &Progress,
    ) -> Result<JobResult> {
        let seed = u64::from_be_bytes(job_id.as_bytes()[..8].try_into().expect("uuid is 16 bytes"));
        let unet = self
            .list_video_models()
            .await
            .into_iter()
            .find(|name| {
                let wanted = params.checkpoint_id.as_deref().unwrap_or("");
                wanted.is_empty()
                    || name.eq_ignore_ascii_case(wanted)
                    || tidy_model_name(name).eq_ignore_ascii_case(wanted)
            })
            .ok_or_else(|| {
                WorkerError::Rejected(
                    "this worker has no video model matching that name".into(),
                )
            })?;

        let info = object_info(&self.http, &self.config).await?;
        if !node_exists(&info, "MiniMaxH3ImageToVideo") {
            return Err(WorkerError::Rejected(
                "ComfyUI does not have MiniMax H3 nodes — update ComfyUI to serve this video model"
                    .into(),
            ));
        }

        let clip = combo(&info, "CLIPLoader", "clip_name")
            .into_iter()
            .find(|n| is_minimax_clip(n))
            .ok_or_else(|| {
                WorkerError::Rejected(
                    "no MiniMax H3 text encoder is installed (models/text_encoders)".into(),
                )
            })?;
        let video_vae = combo(&info, "VAELoader", "vae_name")
            .into_iter()
            .find(|n| is_minimax_video_vae(n))
            .ok_or_else(|| {
                WorkerError::Rejected("no MiniMax H3 video VAE is installed (models/vae)".into())
            })?;
        let audio_vae = combo(&info, "VAELoader", "vae_name")
            .into_iter()
            .find(|n| is_minimax_audio_vae(n));

        let mut graph = minimax_h3_graph(
            &unet,
            &clip,
            &video_vae,
            audio_vae.as_deref(),
            &params.prompt,
            seed,
        );
        if let Some(frame) = &params.from_image {
            let filename = self.upload_image(frame, job_id, "first").await?;
            graph["15"] = serde_json::json!({
                "class_type": "LoadImage",
                "inputs": { "image": filename }
            });
            graph["5"]["inputs"]["first_frame"] = serde_json::json!(["15", 0]);
        }

        let deadline = self.config.timeout_secs.max(1_800);
        let history = self
            .render_with_timeout(&graph, job_id, progress, deadline)
            .await?;
        let (node, media) = first_media(&history).ok_or_else(|| {
            WorkerError::backend(
                "comfyui",
                execution_error(&history).unwrap_or_else(|| {
                    "the workflow finished without producing a video".to_string()
                }),
            )
        })?;
        let bytes = self.fetch_image(&media).await?;
        progress.set(0.99);

        Ok(JobResult {
            v: PROTOCOL_VERSION,
            job_id,
            kind: JobKind::Video,
            sha256: sha256_hex(&bytes),
            text: None,
            tool_calls: Vec::new(),
            image_path_or_b64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            thinking: None,
            meta: serde_json::json!({
                "model": tidy_model_name(&unet),
                "backend": "comfyui",
                "seed": seed,
                "node": node,
                "filename": media.filename,
            }),
        })
    }

    async fn render_with_timeout(
        &self,
        graph: &Value,
        job_id: Uuid,
        progress: &Progress,
        timeout_secs: u64,
    ) -> Result<Value> {
        let client_id = job_id.to_string();
        let mut socket = match tokio_tungstenite::connect_async(self.ws_url(&client_id)).await {
            Ok((ws, _)) => Some(ws),
            Err(e) => {
                tracing::debug!("comfyui progress socket unavailable ({e}); polling only");
                None
            }
        };

        let resp = self
            .http
            .post(self.url("/prompt"))
            .json(&serde_json::json!({ "prompt": graph, "client_id": client_id }))
            .send()
            .await
            .map_err(|e| WorkerError::backend("comfyui", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(400).collect();
            return Err(WorkerError::backend(
                "comfyui",
                format!("HTTP {status}: {snippet}"),
            ));
        }
        let queued: QueueResponse = resp
            .json()
            .await
            .map_err(|e| WorkerError::backend("comfyui", format!("bad /prompt response: {e}")))?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(WorkerError::backend(
                    "comfyui",
                    "timed out waiting for the video",
                ));
            }
            let tick = tokio::time::sleep(Duration::from_millis(750));
            tokio::pin!(tick);
            tokio::select! {
                frame = next_text(&mut socket) => {
                    if let Some(text) = frame {
                        if let Some(fraction) = parse_progress(&text) {
                            progress.set(fraction);
                        }
                    }
                }
                _ = &mut tick => {}
            }
            if let Some(entry) = self.fetch_history(&queued.prompt_id).await? {
                return Ok(entry);
            }
        }
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Next text frame from the progress socket, or never if there is no socket.
async fn next_text(socket: &mut Option<Socket>) -> Option<String> {
    match socket {
        None => std::future::pending().await,
        Some(ws) => match ws.next().await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => Some(t),
            Some(Ok(_)) => Some(String::new()),
            // Socket died: stop watching it, keep polling history.
            _ => {
                *socket = None;
                Some(String::new())
            }
        },
    }
}

/// ComfyUI sends `{"type":"progress","data":{"value":3,"max":20}}`.
fn parse_progress(frame: &str) -> Option<f32> {
    let msg: Value = serde_json::from_str(frame).ok()?;
    if msg.get("type")?.as_str()? != "progress" {
        return None;
    }
    let data = msg.get("data")?;
    let value = data.get("value")?.as_f64()?;
    let max = data.get("max")?.as_f64()?;
    if max <= 0.0 {
        return None;
    }
    Some(((value / max) as f32).clamp(0.0, 0.98))
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ImageRef {
    filename: String,
    #[serde(default)]
    subfolder: String,
    #[serde(default, rename = "type")]
    folder_type: String,
}

/// What ComfyUI said went wrong, if it said anything.
///
/// The history entry carries a `status.messages` list, and a failed run has an
/// `execution_error` in it naming the node that raised and why. That sentence
/// is the difference between "your checkpoint has no text encoder, it needs
/// its own workflow" and an operator restarting things at random.
fn execution_error(history: &Value) -> Option<String> {
    let messages = history.pointer("/status/messages")?.as_array()?;
    for message in messages {
        let pair = message.as_array()?;
        if pair.first()?.as_str()? != "execution_error" {
            continue;
        }
        let detail = pair.get(1)?;
        let reason = detail
            .get("exception_message")
            .and_then(|m| m.as_str())
            .unwrap_or("the node raised, without saying why");
        let node_type = detail
            .get("node_type")
            .and_then(|t| t.as_str())
            .unwrap_or("a node");
        let node_id = detail.get("node_id").and_then(|i| i.as_str()).unwrap_or("?");
        // One line: this is read in a client's error box, not a log viewer.
        let reason: String = reason.split('\n').filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join(" ");
        return Some(format!(
            "{node_type} (node {node_id}) failed: {}",
            reason.chars().take(300).collect::<String>()
        ));
    }
    None
}

fn is_video_model(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "minimax",
        "hailuo",
        "hunyuan",
        "wan",
        "ltx",
        "cogvideo",
        "mochi",
        "svd",
        "kling",
        "fl2va",
        "ref2va",
        "i2v",
        "t2v",
    ]
    .iter()
    .any(|k| n.contains(k))
}

/// MiniMax H3 first/last-frame weights — the graph we actually know how to run.
/// `ref2va` is a different node (reference-to-video) and is not advertised.
fn is_minimax_t2v(name: &str) -> bool {
    if !is_video_model(name) {
        return false;
    }
    let n = name.to_lowercase();
    if n.contains("ref2va") || n.contains("ref2v") {
        return false;
    }
    n.contains("minimax") || n.contains("hailuo") || n.contains("fl2va")
}

fn is_minimax_clip(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("minimax") || n.contains("qwen3vl")
}

fn is_minimax_video_vae(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("minimax") && n.contains("video")
}

fn is_minimax_audio_vae(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("minimax") && n.contains("audio")
}

async fn object_info(http: &reqwest::Client, config: &ComfyConfig) -> Result<Value> {
    let url = ComfyBackend::url_of(&config.endpoint, "/object_info");
    http.get(&url)
        .send()
        .await
        .map_err(|e| WorkerError::backend("comfyui", e))?
        .json()
        .await
        .map_err(|e| WorkerError::backend("comfyui", format!("bad /object_info: {e}")))
}

fn node_exists(info: &Value, class: &str) -> bool {
    info.get(class).is_some()
}

fn combo(info: &Value, class: &str, field: &str) -> Vec<String> {
    info.pointer(&format!("/{class}/input/required/{field}/0"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn minimax_h3_graph(
    unet: &str,
    clip: &str,
    video_vae: &str,
    audio_vae: Option<&str>,
    prompt: &str,
    seed: u64,
) -> Value {
    let mut graph = serde_json::json!({
        "1": { "class_type": "UNETLoader", "inputs": { "unet_name": unet, "weight_dtype": "default" } },
        "2": { "class_type": "CLIPLoader", "inputs": { "clip_name": clip, "type": "minimax", "device": "default" } },
        "3": { "class_type": "VAELoader", "inputs": { "vae_name": video_vae } },
        "5": {
            "class_type": "MiniMaxH3ImageToVideo",
            "inputs": {
                "clip": ["2", 0],
                "vae": ["3", 0],
                "prompt": prompt,
                "width": 864,
                "height": 480,
                "length": 73
            }
        },
        "6": { "class_type": "RandomNoise", "inputs": { "noise_seed": seed } },
        "7": { "class_type": "KSamplerSelect", "inputs": { "sampler_name": "res_multistep" } },
        "8": {
            "class_type": "BasicScheduler",
            "inputs": { "model": ["1", 0], "scheduler": "simple", "steps": 20, "denoise": 1.0 }
        },
        "9": {
            "class_type": "BasicGuider",
            "inputs": { "model": ["1", 0], "conditioning": ["5", 0] }
        },
        "10": {
            "class_type": "SamplerCustomAdvanced",
            "inputs": {
                "noise": ["6", 0],
                "guider": ["9", 0],
                "sampler": ["7", 0],
                "sigmas": ["8", 0],
                "latent_image": ["5", 1]
            }
        },
        "11": { "class_type": "VAEDecode", "inputs": { "samples": ["10", 0], "vae": ["3", 0] } },
        "13": {
            "class_type": "CreateVideo",
            "inputs": { "images": ["11", 0], "fps": 24, "bit_depth": 8 }
        },
        "14": {
            "class_type": "SaveVideo",
            "inputs": { "video": ["13", 0], "filename_prefix": "rootmode", "format": "auto", "codec": "auto" }
        }
    });
    if let Some(audio) = audio_vae {
        graph["4"] = serde_json::json!({ "class_type": "VAELoader", "inputs": { "vae_name": audio } });
        graph["12"] = serde_json::json!({
            "class_type": "VAEDecodeAudio",
            "inputs": { "samples": ["10", 0], "vae": ["4", 0] }
        });
        graph["13"]["inputs"]["audio"] = serde_json::json!(["12", 0]);
    }
    graph
}

fn first_media(history: &Value) -> Option<(String, ImageRef)> {
    let outputs = history.get("outputs")?.as_object()?;
    for key in ["gifs", "videos", "images", "files"] {
        for (node, output) in outputs {
            let Some(items) = output.get(key).and_then(|i| i.as_array()) else {
                continue;
            };
            for item in items {
                if let Ok(parsed) = serde_json::from_value::<ImageRef>(item.clone()) {
                    if !parsed.filename.is_empty() {
                        return Some((node.clone(), parsed));
                    }
                }
            }
        }
    }
    None
}

/// First image any node produced. Workflows often have several save nodes;
/// order is whatever the graph declared.
fn first_image(history: &Value) -> Option<(String, ImageRef)> {
    let outputs = history.get("outputs")?.as_object()?;
    for (node, output) in outputs {
        let Some(images) = output.get("images").and_then(|i| i.as_array()) else {
            continue;
        };
        for image in images {
            if let Ok(parsed) = serde_json::from_value::<ImageRef>(image.clone()) {
                if !parsed.filename.is_empty() {
                    return Some((node.clone(), parsed));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {

    #[test]
    fn minimax_h3_is_a_video_model() {
        assert!(is_video_model("minimax_h3_fl2va_pruned_int8_convrot.safetensors"));
        assert!(is_minimax_t2v("minimax_h3_fl2va_pruned_int8_convrot.safetensors"));
        assert!(!is_minimax_t2v("minimax_h3_ref2va_pruned_int8_convrot.safetensors"));
        assert!(!is_minimax_t2v("sdxl_base_1.0.safetensors"));
        assert!(!is_video_model("sdxl_base_1.0.safetensors"));
    }

    #[test]
    fn first_media_prefers_a_saved_clip() {
        let history = serde_json::json!({
            "outputs": {
                "14": {
                    "gifs": [{
                        "filename": "rootmode_00001.mp4",
                        "subfolder": "video",
                        "type": "output"
                    }]
                }
            }
        });
        let (node, media) = first_media(&history).expect("clip");
        assert_eq!(node, "14");
        assert_eq!(media.filename, "rootmode_00001.mp4");
        assert_eq!(media.folder_type, "output");
    }

    #[test]
    fn a_node_starts_even_when_comfyui_is_not_answering_yet() {
        // ComfyUI is usually launched beside the worker and takes a while to
        // load its models. Exiting here would make an operator start two
        // things in the right order; instead the shelf is empty until the
        // next refresh finds it.
        assert_eq!(pick_checkpoint(&[], ""), "image");
        assert_eq!(pick_checkpoint(&[], "krea2-turbo"), "krea2-turbo");
    }
    use super::*;
    use crate::config::ComfyConfig;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A workflow as an operator would have exported it: their sampler
    /// settings, their size, their negative prompt, all already chosen. Only
    /// the positive prompt is left for the client to fill.
    const TEMPLATE: &str = r#"{
      "3": { "class_type": "KSampler", "inputs": { "seed": 0, "steps": 30, "cfg": 5.5 } },
      "5": { "class_type": "EmptyLatentImage", "inputs": { "width": 768, "height": 1024 } },
      "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "" } },
      "7": { "class_type": "CLIPTextEncode", "inputs": { "text": "blurry" } },
      "4": { "class_type": "CheckpointLoaderSimple", "inputs": { "ckpt_name": "sdxl.safetensors" } }
    }"#;

    fn workflow_file(body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rootmode-wf-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("wf.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn slots() -> BTreeMap<String, String> {
        [("prompt", "6.inputs.text"), ("seed", "3.inputs.seed")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    async fn backend_with(
        endpoint: String,
        workflow: PathBuf,
        slots: BTreeMap<String, String>,
    ) -> Result<ComfyBackend> {
        ComfyBackend::new(ComfyConfig {
            endpoint,
            workflow: Some(workflow),
            workflow_for: Vec::new(),
            checkpoint_id: "sdxl-base-1.0".into(),
            model_hash: None,
            price: None,
            prices: Default::default(),
            currency: "USD".into(),
            slots,
            timeout_secs: 10,
        })
        .await
    }

    fn params() -> ImageParams {
        ImageParams {
            model_hash: None,
            checkpoint_id: None,
            prompt: "a node you own".into(),
            from_image: None,
            change: None,
            mask: None,
        }
    }

    #[tokio::test]
    async fn fills_only_the_declared_slots() {
        let backend = backend_with(
            "http://127.0.0.1:1".into(),
            workflow_file(TEMPLATE),
            slots(),
        )
        .await
        .unwrap();
        let graph = backend.graph_for(&params(), 42).unwrap();

        // The prompt, and the worker's seed. That is the whole of it.
        assert_eq!(graph["6"]["inputs"]["text"], "a node you own");
        assert_eq!(graph["3"]["inputs"]["seed"], 42);

        // Everything else is exactly as the operator exported it. These are
        // decisions about their pipeline, and a client cannot reach them.
        assert_eq!(graph["7"]["inputs"]["text"], "blurry");
        assert_eq!(graph["3"]["inputs"]["steps"], 30);
        assert_eq!(graph["3"]["inputs"]["cfg"], 5.5);
        assert_eq!(graph["5"]["inputs"]["width"], 768);
        assert_eq!(graph["5"]["inputs"]["height"], 1024);
        assert_eq!(graph["4"]["inputs"]["ckpt_name"], "sdxl.safetensors");
    }

    #[tokio::test]
    async fn a_prompt_cannot_reach_beyond_its_slot() {
        let backend = backend_with(
            "http://127.0.0.1:1".into(),
            workflow_file(TEMPLATE),
            slots(),
        )
        .await
        .unwrap();
        let mut evil = params();
        evil.prompt = r#"{"4": {"inputs": {"ckpt_name": "/etc/passwd"}}}"#.into();

        let graph = backend.graph_for(&evil, 1).unwrap();
        // The payload lands as a *string* in the text slot, not as structure.
        assert!(graph["6"]["inputs"]["text"].is_string());
        assert_eq!(graph["4"]["inputs"]["ckpt_name"], "sdxl.safetensors");
        assert_eq!(graph.as_object().unwrap().len(), 5, "no nodes were added");
    }

    #[tokio::test]
    async fn a_slot_that_does_not_exist_fails_at_startup() {
        let mut bad = slots();
        bad.insert("seed".into(), "99.inputs.seed".into());
        let err = backend_with("http://127.0.0.1:1".into(), workflow_file(TEMPLATE), bad)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[tokio::test]
    async fn rejects_a_workflow_that_is_not_api_format() {
        let err = backend_with(
            "http://127.0.0.1:1".into(),
            workflow_file("[1,2,3]"),
            slots(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("API-format"), "got: {err}");
    }

    #[tokio::test]
    async fn the_shipped_workflow_matches_the_documented_slots() {
        // The example config in `config.rs` tells operators to use these slot
        // paths against `workflows/sdxl_txt2img.json`. If either drifts, the
        // documentation is wrong and this fails.
        let workflow =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workflows/sdxl_txt2img.json");
        let backend = backend_with("http://127.0.0.1:1".into(), workflow, slots())
            .await
            .unwrap();

        let graph = backend.graph_for(&params(), 7).unwrap();
        assert_eq!(graph["6"]["inputs"]["text"], "a node you own");
        assert_eq!(graph["3"]["inputs"]["seed"], 7);
        // The size is the workflow's, not the client's.
        assert!(graph["5"]["inputs"]["width"].is_number());
        // Wiring between nodes is untouched by slot filling.
        assert_eq!(graph["3"]["inputs"]["model"], serde_json::json!(["4", 0]));
    }

    #[test]
    fn the_generated_graph_is_complete_and_fillable() {
        // Pointing at an endpoint has to be the whole configuration for the
        // ordinary case, so the graph the worker builds when no workflow is
        // given must be a working one — and its slots must land where the
        // built-in slot map says they do.
        let graph = default_graph("my-model.safetensors");

        assert_eq!(graph["4"]["inputs"]["ckpt_name"], "my-model.safetensors");
        for node in ["3", "4", "5", "6", "7", "8", "9"] {
            assert!(
                graph[node]["class_type"].is_string(),
                "node {node} is missing"
            );
        }
        // Wired end to end: sampler reads the model, prompts and latent; the
        // decode reads the sampler; the save reads the decode.
        assert_eq!(graph["3"]["inputs"]["model"], serde_json::json!(["4", 0]));
        assert_eq!(
            graph["3"]["inputs"]["positive"],
            serde_json::json!(["6", 0])
        );
        assert_eq!(
            graph["3"]["inputs"]["negative"],
            serde_json::json!(["7", 0])
        );
        assert_eq!(graph["8"]["inputs"]["samples"], serde_json::json!(["3", 0]));
        assert_eq!(graph["9"]["inputs"]["images"], serde_json::json!(["8", 0]));

        // Every default slot resolves against it.
        for (field, path) in default_slots() {
            let mut probe = graph.clone();
            navigate(&mut probe, &path)
                .unwrap_or_else(|_| panic!("slot '{field}' -> '{path}' is not in the graph"));
        }
    }

    #[test]
    fn starting_from_a_picture_changes_only_the_latent_and_the_denoise() {
        // Everything that makes the picture — the checkpoint, the sampler,
        // the size — stays exactly as it was. The image comes in as the
        // starting latent, and the sampler is told to keep most of it.
        let plain = default_graph("m.safetensors");
        let from = img2img_graph("m.safetensors", "rootmode-abc.png", 0.4);

        assert_eq!(from["10"]["inputs"]["image"], "rootmode-abc.png");
        assert_eq!(from["11"]["inputs"]["pixels"], serde_json::json!(["10", 0]));
        // The sampler now reads the encoded picture, not empty noise.
        assert_eq!(
            from["3"]["inputs"]["latent_image"],
            serde_json::json!(["11", 0])
        );
        // f32 → JSON widens to f64, so compare with a tolerance rather than
        // asserting on the exact bits.
        let denoise = from["3"]["inputs"]["denoise"].as_f64().unwrap();
        assert!((denoise - 0.4).abs() < 1e-6, "denoise was {denoise}");
        assert_eq!(
            plain["3"]["inputs"]["latent_image"],
            serde_json::json!(["5", 0])
        );

        // Untouched.
        assert_eq!(from["4"], plain["4"]);
        assert_eq!(from["3"]["inputs"]["steps"], plain["3"]["inputs"]["steps"]);
        assert_eq!(from["9"], plain["9"]);

        // The prompt still lands where the default slots say it does.
        for (field, path) in default_slots() {
            let mut probe = from.clone();
            navigate(&mut probe, &path)
                .unwrap_or_else(|_| panic!("slot '{field}' -> '{path}' missing from img2img"));
        }
    }

    #[test]
    fn a_mask_routes_the_latent_through_it_and_leaves_the_rest_alone() {
        // The whole promise of inpainting is that everything outside the mask
        // is untouched. That rests on the sampler reading the masked latent
        // rather than the plain one.
        let plain = img2img_graph("m.safetensors", "from.png", 0.9);
        let masked = inpaint_graph("m.safetensors", "from.png", "mask.png", 0.9);

        assert_eq!(masked["12"]["inputs"]["image"], "mask.png");
        assert_eq!(masked["12"]["inputs"]["channel"], "red");
        // Grown, so the repaint blends instead of leaving a seam.
        assert_eq!(masked["13"]["inputs"]["mask"], serde_json::json!(["12", 0]));
        assert!(masked["13"]["inputs"]["expand"].as_i64().unwrap() > 0);
        assert_eq!(
            masked["14"]["inputs"]["samples"],
            serde_json::json!(["11", 0])
        );
        assert_eq!(masked["14"]["inputs"]["mask"], serde_json::json!(["13", 0]));

        // The sampler reads through the mask; without one it reads the picture
        // directly and would repaint everything.
        assert_eq!(
            masked["3"]["inputs"]["latent_image"],
            serde_json::json!(["14", 0])
        );
        assert_eq!(
            plain["3"]["inputs"]["latent_image"],
            serde_json::json!(["11", 0])
        );

        // What is saved is the original with only the masked part replaced,
        // so the untouched region is the client's own pixels rather than a
        // lossy round trip of them.
        assert_eq!(
            masked["15"]["inputs"]["destination"],
            serde_json::json!(["10", 0])
        );
        assert_eq!(
            masked["15"]["inputs"]["source"],
            serde_json::json!(["8", 0])
        );
        assert_eq!(
            masked["9"]["inputs"]["images"],
            serde_json::json!(["15", 0])
        );
        assert_eq!(plain["9"]["inputs"]["images"], serde_json::json!(["8", 0]));

        // The picture itself still arrives the same way.
        assert_eq!(masked["10"], plain["10"]);
        assert_eq!(masked["11"], plain["11"]);
        assert_eq!(masked["4"], plain["4"]);
    }

    #[test]
    fn how_much_to_change_has_a_sane_default_and_real_bounds() {
        // A client cannot calibrate this for a checkpoint it has never seen,
        // so silence has to mean something reasonable.
        assert_eq!(change_to_denoise(None), 0.45);
        assert_eq!(change_to_denoise(Some(0.8)), 0.8);
        // Zero would return the picture untouched, which is never what was
        // meant by asking for a change.
        assert_eq!(change_to_denoise(Some(0.0)), 0.05);
        assert_eq!(change_to_denoise(Some(5.0)), 1.0);
    }

    #[test]
    fn a_checkpoint_filename_becomes_a_name_someone_would_type() {
        // An unnamed backend advertises what the server had. Advertising the
        // raw filename — or worse, an empty string — gives clients something
        // they cannot sensibly ask for.
        assert_eq!(
            tidy_model_name("lustifyNSFWCheckpoint_ggwpV7.safetensors"),
            "lustifyNSFWCheckpoint_ggwpV7"
        );
        assert_eq!(
            tidy_model_name("sd_xl_base_1.0.safetensors"),
            "sd_xl_base_1.0"
        );
        assert_eq!(tidy_model_name("SDXL/turbo.ckpt"), "turbo");
        // Already clean names are left alone.
        assert_eq!(tidy_model_name("my-model"), "my-model");
    }

    #[test]
    fn reads_comfy_progress_frames() {
        assert_eq!(
            parse_progress(r#"{"type":"progress","data":{"value":5,"max":20}}"#),
            Some(0.25)
        );
        assert_eq!(parse_progress(r#"{"type":"executing","data":{}}"#), None);
        assert_eq!(parse_progress("not json"), None);
    }

    #[test]
    fn finds_the_first_saved_image() {
        let history = serde_json::json!({
            "outputs": {
                "9": { "images": [{ "filename": "out_001.png", "subfolder": "", "type": "output" }] }
            }
        });
        let (node, image) = first_image(&history).unwrap();
        assert_eq!(node, "9");
        assert_eq!(image.filename, "out_001.png");

        assert!(first_image(&serde_json::json!({ "outputs": {} })).is_none());
    }

    /// What `/object_info/CheckpointLoaderSimple` looks like from a box with
    /// several checkpoints installed.
    fn object_info(names: &[&str]) -> String {
        serde_json::json!({
            "CheckpointLoaderSimple": {
                "input": { "required": { "ckpt_name": [names] } }
            }
        })
        .to_string()
    }

    async fn generic_backend(endpoint: String, configured: &str) -> Result<ComfyBackend> {
        ComfyBackend::new(ComfyConfig {
            endpoint,
            workflow: None,
            workflow_for: Vec::new(),
            checkpoint_id: configured.into(),
            model_hash: None,
            price: None,
            prices: Default::default(),
            currency: "USD".into(),
            slots: Default::default(),
            timeout_secs: 10,
        })
        .await
    }

    #[tokio::test]
    async fn advertises_every_checkpoint_the_box_has() {
        let listing = object_info(&["sdxl.safetensors", "flux1-dev.safetensors", "pony.ckpt"]);
        let stub = crate::testutil::StubHttp::start(vec![
            crate::testutil::StubHttp::json(200, &listing),
            crate::testutil::StubHttp::json(200, &listing),
        ])
        .await;

        let backend = generic_backend(stub.base_url(), "").await.unwrap();
        let models = backend.discover_models().await.unwrap();

        // Not just the default. Installing a checkpoint is all it should take
        // to be able to serve it.
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["sdxl", "flux1-dev", "pony"]);
        assert!(models.iter().all(|m| m.price.is_none()));
    }

    #[tokio::test]
    async fn each_checkpoint_can_have_its_own_price() {
        let listing = object_info(&["sdxl.safetensors", "flux1-dev.safetensors", "pony.ckpt"]);
        let stub = crate::testutil::StubHttp::start(vec![
            crate::testutil::StubHttp::json(200, &listing),
            crate::testutil::StubHttp::json(200, &listing),
        ])
        .await;

        let mut prices = BTreeMap::new();
        prices.insert("flux1-dev".into(), 0.05);
        prices.insert("pony".into(), 0.01);
        let backend = ComfyBackend::new(ComfyConfig {
            endpoint: stub.base_url(),
            workflow: None,
            workflow_for: Vec::new(),
            checkpoint_id: String::new(),
            model_hash: None,
            price: Some(0.02),
            prices,
            currency: "USD".into(),
            slots: Default::default(),
            timeout_secs: 10,
        })
        .await
        .unwrap();

        let models = backend.discover_models().await.unwrap();
        let amount = |id: &str| {
            models
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.amount())
                .unwrap()
        };
        assert_eq!(amount("sdxl"), 0.02, "the default covers unlisted models");
        assert_eq!(amount("flux1-dev"), 0.05);
        assert_eq!(amount("pony"), 0.01);
    }

    #[test]
    fn a_price_key_matches_the_advertised_name_or_a_prefix() {
        let mut prices = BTreeMap::new();
        prices.insert("krea2".into(), 0.08);
        assert_eq!(lookup_price("krea2-turbo", None, &prices), Some(0.08));
        assert_eq!(lookup_price("sdxl", Some(0.02), &prices), Some(0.02));
    }

    #[tokio::test]
    async fn a_job_picks_which_checkpoint_it_wants() {
        let listing = object_info(&["sdxl.safetensors", "flux1-dev.safetensors"]);
        let stub = crate::testutil::StubHttp::start(vec![crate::testutil::StubHttp::json(
            200, &listing,
        )])
        .await;
        let backend = generic_backend(stub.base_url(), "").await.unwrap();

        // Named the way it was advertised — tidied, without the extension.
        let mut asked = params();
        asked.checkpoint_id = Some("flux1-dev".into());
        let graph = backend.graph_for(&asked, 7).unwrap();
        assert_eq!(graph["4"]["inputs"]["ckpt_name"], "flux1-dev.safetensors");

        // Naming nothing still gets the default, so an older client that does
        // not know about the field keeps working.
        let graph = backend.graph_for(&params(), 7).unwrap();
        assert_eq!(graph["4"]["inputs"]["ckpt_name"], "sdxl.safetensors");
    }

    #[tokio::test]
    async fn a_checkpoint_the_box_does_not_have_is_refused() {
        let listing = object_info(&["sdxl.safetensors"]);
        let stub = crate::testutil::StubHttp::start(vec![crate::testutil::StubHttp::json(
            200, &listing,
        )])
        .await;
        let backend = generic_backend(stub.base_url(), "").await.unwrap();

        let mut asked = params();
        asked.checkpoint_id = Some("midjourney".into());
        let err = backend.graph_for(&asked, 1).unwrap_err().to_string();

        // Rendering with different weights and saying nothing would come back
        // looking wrong with no way to tell why.
        assert!(err.contains("midjourney"), "got: {err}");
        assert!(err.contains("sdxl"), "should say what it does have: {err}");
    }

    #[tokio::test]
    async fn an_operator_workflow_still_advertises_exactly_one_model() {
        let backend = backend_with(
            "http://127.0.0.1:1".into(),
            workflow_file(TEMPLATE),
            slots(),
        )
        .await
        .unwrap();
        let models = backend.discover_models().await.unwrap();

        // Their graph loads the checkpoint it names; the others on the box are
        // not reachable through it.
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "sdxl-base-1.0");
    }

    #[tokio::test]
    async fn a_checkpoint_added_after_boot_becomes_servable() {
        let stub = crate::testutil::StubHttp::start(vec![
            crate::testutil::StubHttp::json(200, &object_info(&["sdxl.safetensors"])),
            crate::testutil::StubHttp::json(
                200,
                &object_info(&["sdxl.safetensors", "flux1-dev.safetensors"]),
            ),
        ])
        .await;

        let backend = generic_backend(stub.base_url(), "").await.unwrap();
        let models = backend.discover_models().await.unwrap();
        assert_eq!(models.len(), 2, "the second listing is read, not cached");

        let mut asked = params();
        asked.checkpoint_id = Some("flux1-dev".into());
        assert_eq!(
            backend.graph_for(&asked, 1).unwrap()["4"]["inputs"]["ckpt_name"],
            "flux1-dev.safetensors"
        );
    }

    #[test]
    fn a_checkpoint_resolves_by_filename_tidy_name_or_prefix() {
        let have = vec!["sdxl-base-1.0.safetensors".to_string(), "pony.ckpt".to_string()];
        for name in ["sdxl-base-1.0.safetensors", "sdxl-base-1.0", "SDXL-BASE", "sdxl"] {
            assert_eq!(
                match_checkpoint(&have, name).as_deref(),
                Some("sdxl-base-1.0.safetensors"),
                "{name} should resolve"
            );
        }
        assert_eq!(match_checkpoint(&have, "").as_deref(), Some("sdxl-base-1.0.safetensors"));
        assert!(match_checkpoint(&have, "nothing-like-it").is_none());
    }

    #[test]
    fn a_distilled_checkpoint_does_not_get_sdxl_guidance() {
        // cfg 6 on Flux comes back burnt, and it reads as a broken network
        // rather than a setting that does not suit the model.
        for name in ["flux1-dev.safetensors", "sd3.5_large.safetensors", "dreamshaperXL_turbo.safetensors"] {
            assert_eq!(guidance_for(name), 1.0, "{name}");
        }
        for name in ["sdxl-base-1.0.safetensors", "realisticVision_v6.safetensors"] {
            assert_eq!(guidance_for(name), 6.0, "{name}");
        }
        // It reaches the graph, not just the helper.
        assert_eq!(default_graph("flux1-dev.safetensors")["3"]["inputs"]["cfg"], 1.0);
        assert_eq!(default_graph("sdxl.safetensors")["3"]["inputs"]["cfg"], 6.0);
    }

    #[test]
    fn a_failed_run_reports_what_comfyui_actually_said() {
        // The shape ComfyUI returns when a node raises. Reporting "no image"
        // instead of this sends an operator looking at the wrong thing —
        // here, at the network, when the answer is that the checkpoint has no
        // text encoder and needs its own workflow.
        let history = serde_json::json!({
            "status": {
                "status_str": "error",
                "completed": false,
                "messages": [
                    ["execution_start", { "prompt_id": "abc" }],
                    ["execution_error", {
                        "node_id": "6",
                        "node_type": "CLIPTextEncode",
                        "exception_message":
                            "ERROR: clip input is invalid: None\n\nIf the clip is from a checkpoint loader node your checkpoint does not contain a valid clip or text encoder model."
                    }]
                ]
            },
            "outputs": {}
        });

        let said = execution_error(&history).expect("an error was reported");
        assert!(said.contains("CLIPTextEncode"), "names the node: {said}");
        assert!(said.contains("node 6"), "names which one: {said}");
        assert!(said.contains("does not contain a valid clip"), "keeps the reason: {said}");
        assert!(!said.contains('\n'), "one line, for a client error box: {said}");
    }

    #[test]
    fn a_run_that_simply_produced_nothing_has_no_error_to_report() {
        let history = serde_json::json!({
            "status": { "status_str": "success", "messages": [["execution_start", {}]] },
            "outputs": {}
        });
        assert!(execution_error(&history).is_none());
        assert!(execution_error(&serde_json::json!({})).is_none());
    }

    /// A Flux-shaped graph: the text encoder is a separate loader, so this
    /// cannot be the built-in graph with a different ckpt_name.
    const FLUX_TEMPLATE: &str = r#"{
      "3": { "class_type": "KSampler", "inputs": { "seed": 0, "steps": 8, "cfg": 1.0 } },
      "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "", "clip": ["11", 0] } },
      "11": { "class_type": "DualCLIPLoader",
              "inputs": { "clip_name1": "t5xxl.safetensors", "clip_name2": "clip_l.safetensors" } },
      "12": { "class_type": "UNETLoader", "inputs": { "unet_name": "krea2.safetensors" } },
      "9": { "class_type": "SaveImage", "inputs": { "filename_prefix": "rootmode" } }
    }"#;

    async fn two_pipelines(endpoint: String, krea: PathBuf) -> Result<ComfyBackend> {
        ComfyBackend::new(ComfyConfig {
            endpoint,
            workflow: None,
            workflow_for: vec![crate::config::WorkflowChoice {
                model: "krea2-turbo".into(),
                file: krea,
                slots: Default::default(),
            }],
            checkpoint_id: String::new(),
            model_hash: None,
            price: None,
            prices: Default::default(),
            currency: "USD".into(),
            slots: Default::default(),
            timeout_secs: 10,
        })
        .await
    }

    #[tokio::test]
    async fn each_model_gets_the_graph_built_for_it() {
        let listing = object_info(&["lustify.safetensors"]);
        let stub = crate::testutil::StubHttp::start(vec![
            crate::testutil::StubHttp::json(200, &listing),
            crate::testutil::StubHttp::json(200, &listing),
        ])
        .await;
        let backend = two_pipelines(stub.base_url(), workflow_file(FLUX_TEMPLATE))
            .await
            .unwrap();

        // Both are on offer: the one with its own pipeline, and the one the
        // built-in graph can serve.
        let ids: Vec<String> = backend
            .discover_models()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert!(ids.contains(&"krea2-turbo".to_string()), "{ids:?}");
        assert!(ids.contains(&"lustify".to_string()), "{ids:?}");

        // Asking for krea2 runs the operator's Flux graph — external text
        // encoder and all — not the built-in one with a swapped checkpoint.
        let mut asked = params();
        asked.checkpoint_id = Some("krea2-turbo".into());
        let graph = backend.graph_for(&asked, 5).unwrap();
        assert_eq!(graph["11"]["class_type"], "DualCLIPLoader");
        assert_eq!(graph["6"]["inputs"]["text"], "a node you own");
        assert_eq!(graph["3"]["inputs"]["seed"], 5);
        assert!(graph.get("4").is_none(), "no CheckpointLoaderSimple: {graph}");

        // Asking for the other one gets the built-in graph, loading it.
        let mut other = params();
        other.checkpoint_id = Some("lustify".into());
        let graph = backend.graph_for(&other, 5).unwrap();
        assert_eq!(graph["4"]["class_type"], "CheckpointLoaderSimple");
        assert_eq!(graph["4"]["inputs"]["ckpt_name"], "lustify.safetensors");
    }

    #[tokio::test]
    async fn a_model_with_its_own_graph_is_not_also_offered_through_the_builtin_one() {
        // The checkpoint file is on disk *and* has an exported pipeline. It
        // must be advertised once: offering both would let a client pick the
        // built-in graph for a model that needs the exported one — which is
        // exactly the failure this whole path exists to avoid.
        let listing = object_info(&["krea2-turbo.safetensors", "lustify.safetensors"]);
        let stub = crate::testutil::StubHttp::start(vec![crate::testutil::StubHttp::json(
            200, &listing,
        )])
        .await;
        let backend = two_pipelines(stub.base_url(), workflow_file(FLUX_TEMPLATE))
            .await
            .unwrap();

        let ids: Vec<String> = backend
            .discover_models()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids.iter().filter(|i| i.eq_ignore_ascii_case("krea2-turbo")).count(), 1, "{ids:?}");
    }

    #[tokio::test]
    async fn a_bad_slot_in_a_per_model_workflow_stops_the_node_starting() {
        let listing = object_info(&["lustify.safetensors"]);
        let stub = crate::testutil::StubHttp::start(vec![crate::testutil::StubHttp::json(
            200, &listing,
        )])
        .await;
        let err = ComfyBackend::new(ComfyConfig {
            endpoint: stub.base_url(),
            workflow: None,
            workflow_for: vec![crate::config::WorkflowChoice {
                model: "krea2-turbo".into(),
                file: workflow_file(FLUX_TEMPLATE),
                slots: [("prompt".to_string(), "99.inputs.text".to_string())]
                    .into_iter()
                    .collect(),
            }],
            checkpoint_id: String::new(),
            model_hash: None,
            price: None,
            prices: Default::default(),
            currency: "USD".into(),
            slots: Default::default(),
            timeout_secs: 10,
        })
        .await
        .err()
        .expect("a workflow with a slot that does not exist must not start")
        .to_string();

        // A typo is a refusal to boot, not a render that fails at 3am.
        assert!(err.contains("krea2-turbo"), "says which workflow: {err}");
        assert!(err.contains("99.inputs.text"), "says which slot: {err}");
    }

    /// The history ComfyUI returns when an all-in-one graph meets a
    /// diffusion-only checkpoint — the exact failure a Krea/Flux file gives.
    fn clip_failure() -> serde_json::Value {
        serde_json::json!({
            "status": { "status_str": "error", "completed": false, "messages": [
                ["execution_error", {
                    "node_id": "6", "node_type": "CLIPTextEncode",
                    "exception_message": "ERROR: clip input is invalid: None\n\nIf the clip is from a checkpoint loader node your checkpoint does not contain a valid clip or text encoder model."
                }]
            ]},
            "outputs": {}
        })
    }

    #[test]
    fn a_missing_text_encoder_is_the_one_failure_worth_retrying() {
        assert!(needs_split(&clip_failure()), "rebuild and try the other shape");

        // Anything else is the operator's problem to read, not ours to guess
        // at: retrying a different graph would only hide it.
        let out_of_memory = serde_json::json!({
            "status": { "messages": [["execution_error", {
                "node_id": "3", "node_type": "KSampler",
                "exception_message": "CUDA out of memory"
            }]]}
        });
        assert!(!needs_split(&out_of_memory));
        assert!(!needs_split(&serde_json::json!({ "status": { "messages": [] } })));
    }

    #[test]
    fn a_split_graph_loads_the_model_encoder_and_vae_separately() {
        let clip = ClipChoice::Dual {
            t5: "t5xxl_fp8_e4m3fn.safetensors".into(),
            l: "clip_l.safetensors".into(),
            kind: "flux".into(),
        };
        let graph = split_graph(
            "krea2.safetensors",
            &clip,
            "ae.safetensors",
            1.0,
            &uiformat::Recipe::default(),
        );

        // No CheckpointLoaderSimple anywhere: that is the node that could not
        // supply a text encoder in the first place.
        assert_eq!(graph["4"]["class_type"], "UNETLoader");
        assert_eq!(graph["11"]["class_type"], "DualCLIPLoader");
        assert_eq!(graph["11"]["inputs"]["clip_name1"], "t5xxl_fp8_e4m3fn.safetensors");
        assert_eq!(graph["11"]["inputs"]["type"], "flux");
        assert_eq!(graph["10"]["class_type"], "VAELoader");
        assert_eq!(graph["8"]["inputs"]["vae"], serde_json::json!(["10", 0]));

        // Node ids stay where the built-in graph puts them, so the prompt and
        // seed slots keep working without a special case.
        assert_eq!(graph["6"]["class_type"], "CLIPTextEncode");
        assert!(graph["3"]["inputs"]["seed"].is_number());
        assert_eq!(graph["3"]["inputs"]["cfg"], 1.0);
        // Four-channel latents unless a recipe says otherwise.
        assert_eq!(graph["5"]["class_type"], "EmptyLatentImage");
    }

    #[test]
    fn the_models_own_template_decides_the_latent_and_the_sampler() {
        // Exactly what ComfyUI's krea2 template says. Getting the latent wrong
        // is what made pictures come back pixelated, and the VAE wrong is what
        // made them look like negatives.
        let recipe = uiformat::Recipe {
            family: Some("krea2".into()),
            model: Some("krea2_turbo_fp8_scaled.safetensors".into()),
            vae: Some("qwen_image_vae.safetensors".into()),
            latent: "EmptySD3LatentImage".into(),
            steps: Some(10),
            cfg: Some(1.0),
            sampler: Some("euler".into()),
            scheduler: Some("simple".into()),
        };
        let clip = ClipChoice::Single {
            name: "qwen3vl_4b_fp8_scaled.safetensors".into(),
            kind: "krea2".into(),
        };
        let graph = split_graph("krea2.safetensors", &clip, "qwen_image_vae.safetensors", 6.0, &recipe);

        assert_eq!(graph["5"]["class_type"], "EmptySD3LatentImage", "sixteen channels");
        assert_eq!(graph["10"]["inputs"]["vae_name"], "qwen_image_vae.safetensors");
        assert_eq!(graph["3"]["inputs"]["steps"], 10);
        assert_eq!(graph["3"]["inputs"]["scheduler"], "simple");
        // The template's own cfg wins over this worker's filename guess.
        assert_eq!(graph["3"]["inputs"]["cfg"], 1.0);
    }

    #[test]
    fn encoders_are_chosen_by_what_the_box_actually_has() {
        let flux_box = Parts {
            clips: vec!["t5xxl_fp8_e4m3fn.safetensors".into(), "clip_l.safetensors".into()],
            vaes: vec!["ae.safetensors".into(), "pixel_space".into()],
            unets: vec!["krea2.safetensors".into()],
            clip_types: Vec::new(),
        };
        assert_eq!(
            flux_box.flux_pair(),
            Some(("t5xxl_fp8_e4m3fn.safetensors".into(), "clip_l.safetensors".into())),
            "t5 first, as DualCLIPLoader expects"
        );
        assert_eq!(flux_box.vae().as_deref(), Some("ae.safetensors"));

        // sparky1's actual situation: a t5 but no clip_l. There is no honest
        // pair to build, so the caller must say what is missing rather than
        // guess an encoder that will not work.
        let half_equipped = Parts {
            clips: vec!["t5xxl_fp8_e4m3fn.safetensors".into(), "qwen3vl_4b_fp8_scaled.safetensors".into()],
            vaes: vec!["ae.safetensors".into()],
            unets: vec![],
            clip_types: Vec::new(),
        };
        assert_eq!(half_equipped.flux_pair(), None);
        assert!(half_equipped.single_clip().is_some(), "one is still better than none");

        // "pixel_space" is a mode, not a file.
        let odd = Parts {
            clips: vec![],
            vaes: vec!["pixel_space".into()],
            unets: vec![],
            clip_types: Vec::new(),
        };
        assert_eq!(odd.vae(), None);
    }

    #[test]
    fn a_checkpoint_finds_its_diffusion_only_twin_by_the_words_they_share() {
        // sparky1's real pair: the same weights, filed under two names.
        let installed = vec!["lustify-v10-krea-turbo-int8_convrot.safetensors".to_string()];
        assert_eq!(
            best_match("krea2TurboOfficialComfy_krea2RawInt8Convrot.safetensors", &installed).as_deref(),
            Some("lustify-v10-krea-turbo-int8_convrot.safetensors"),
            "krea + turbo + int8 + convrot is plenty of signal"
        );

        // camelCase is split, so `krea2Turbo` matches `krea-turbo`.
        assert_eq!(
            best_match("krea2Turbo.safetensors", &["krea-turbo-fp8.safetensors".to_string()]).as_deref(),
            Some("krea-turbo-fp8.safetensors")
        );
    }

    #[test]
    fn an_unrelated_file_is_not_matched_just_because_it_is_the_only_one() {
        // One weak overlap is not a match. Guessing here would render with
        // entirely different weights and call it success, which is worse than
        // a clear failure.
        let installed = vec!["wan2.1_t2v_14B_fp8.safetensors".to_string()];
        assert_eq!(best_match("sdxl-base-1.0.safetensors", &installed), None);
        assert_eq!(best_match("anything.safetensors", &[]), None);

        // Shared noise words alone never match.
        assert_eq!(
            best_match("modelA_fp8.safetensors", &["modelB_fp8.safetensors".to_string()]),
            None,
            "'fp8' and 'model' identify nothing"
        );
    }

    #[test]
    fn a_models_encoder_family_is_read_off_its_name() {
        // The real list from a current ComfyUI, which grows every release —
        // hence reading it rather than shipping a copy.
        let types: Vec<String> = ["stable_diffusion", "sd3", "flux", "qwen_image", "krea2", "ace", "chroma"]
            .iter().map(|s| s.to_string()).collect();

        assert_eq!(
            clip_type_for("krea2TurboOfficialComfy_krea2RawInt8Convrot.safetensors", &types).as_deref(),
            Some("krea2")
        );
        assert_eq!(clip_type_for("qwen_image_fp8.safetensors", &types).as_deref(), Some("qwen_image"));

        // Nothing in the name: the caller falls back rather than guessing.
        assert_eq!(clip_type_for("lustifyNSFWCheckpoint_ggwpV7.safetensors", &types), None);
        // Short types are ignored, or "ace" matches "surface" and "palace".
        assert_eq!(clip_type_for("surface-model.safetensors", &types), None);
    }

    #[test]
    fn comfyui_naming_the_family_it_wants_is_an_instruction_to_follow() {
        let told = serde_json::json!({
            "status": { "messages": [["execution_error", {
                "node_id": "3", "node_type": "KSampler",
                "exception_message": "Krea2 expects conditioning with 12x2560=30720 features (a 12-layer Qwen3-VL stack) but got 2560. Load the text encoder with CLIPLoader type 'krea2'."
            }]]}
        });
        assert_eq!(suggested_clip_type(&told).as_deref(), Some("krea2"));

        // An error that names no family leaves nothing to retry with.
        let vague = serde_json::json!({
            "status": { "messages": [["execution_error", {
                "node_id": "3", "node_type": "KSampler", "exception_message": "CUDA out of memory"
            }]]}
        });
        assert_eq!(suggested_clip_type(&vague), None);
    }

    #[test]
    fn a_family_gets_the_encoder_it_is_built_on() {
        // sparky1's shelf: a Qwen encoder and a T5.
        let parts = Parts {
            clips: vec!["qwen3vl_4b_fp8_scaled.safetensors".into(), "t5xxl_fp8_e4m3fn.safetensors".into()],
            vaes: vec!["ae.safetensors".into()],
            unets: vec![],
            clip_types: vec!["krea2".into(), "flux".into()],
        };
        // Krea2 is a Qwen3-VL stack, so the T5 sitting next to it is wrong
        // even though it is the more familiar file.
        assert_eq!(parts.clip_for("krea2").as_deref(), Some("qwen3vl_4b_fp8_scaled.safetensors"));
        assert_eq!(parts.clip_for("flux").as_deref(), Some("t5xxl_fp8_e4m3fn.safetensors"));
        // An unknown family still gets a candidate rather than nothing.
        assert!(parts.clip_for("something_new").is_some());
    }

    #[tokio::test]
    async fn a_model_this_node_cannot_render_stops_being_advertised() {
        let listing = object_info(&["works.safetensors", "broken.safetensors"]);
        // What one discovery asks for, in order: the checkpoints, the four
        // loader catalogues, the operator's saved workflows (none here),
        // then the video-model object_info.
        let round = || {
            vec![
                crate::testutil::StubHttp::json(200, &listing),
                crate::testutil::StubHttp::json(200, "{}"),
                crate::testutil::StubHttp::json(200, "{}"),
                crate::testutil::StubHttp::json(200, "{}"),
                crate::testutil::StubHttp::json(200, "{}"),
                crate::testutil::StubHttp::json(200, "[]"),
                crate::testutil::StubHttp::json(200, "{}"),
                crate::testutil::StubHttp::json(200, "{}"),
            ]
        };
        let mut responses = vec![crate::testutil::StubHttp::json(200, &listing)]; // new()
        responses.extend(round());
        responses.extend(round());
        let stub = crate::testutil::StubHttp::start(responses).await;
        let backend = generic_backend(stub.base_url(), "").await.unwrap();

        let before: Vec<String> = backend.discover_models().await.unwrap()
            .into_iter().map(|m| m.id).collect();
        assert!(before.contains(&"broken".to_string()), "{before:?}");

        // What a node learns from a job it could not serve.
        backend.retire("broken.safetensors", "KSampler (node 3) failed: unsupported model");

        let after: Vec<String> = backend.discover_models().await.unwrap()
            .into_iter().map(|m| m.id).collect();
        assert!(after.contains(&"works".to_string()), "the rest still stand: {after:?}");
        assert!(
            !after.contains(&"broken".to_string()),
            "a peer must not keep offering what it cannot do: {after:?}"
        );
    }

    #[test]
    fn installing_the_missing_piece_puts_a_retired_model_back_on_offer() {
        let bare = Parts {
            clips: vec![],
            vaes: vec!["ae.safetensors".into()],
            unets: vec!["krea2.safetensors".into()],
            clip_types: vec![],
        };
        let backend = ComfyBackend {
            config: ComfyConfig {
                endpoint: "http://127.0.0.1:1".into(),
                workflow: None,
                workflow_for: vec![],
                checkpoint_id: String::new(),
                model_hash: None,
                price: None,
                prices: Default::default(),
                currency: "USD".into(),
                slots: Default::default(),
                timeout_secs: 5,
            },
            model_id: "x".into(),
            installed: Default::default(),
            template: serde_json::json!({}),
            per_model: vec![],
            shapes: Default::default(),
            clip_kinds: Default::default(),
            unservable: Default::default(),
            shelf: Default::default(),
            saved: Default::default(),
            http: reqwest::Client::new(),
        };

        backend.refresh_shelf(&bare);
        backend.retire("krea2.safetensors", "no text encoder installed");
        assert!(!backend.unservable.read().unwrap().is_empty());

        // The operator drops the encoder in. Nothing restarts; the next
        // discovery notices the shelf changed and asks again.
        let equipped = Parts {
            clips: vec!["qwen3vl.safetensors".into()],
            ..bare.clone()
        };
        backend.refresh_shelf(&equipped);
        assert!(
            backend.unservable.read().unwrap().is_empty(),
            "a fixed box gets its models back without a restart"
        );
    }
}
