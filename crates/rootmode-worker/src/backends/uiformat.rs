//! Turning a workflow saved in ComfyUI's web UI into one `/prompt` will run.
//!
//! The point is that an operator should not have to do anything. They already
//! built graphs in the UI that work on their box, for the models they actually
//! have; those are saved on the server and readable over the same HTTP API the
//! worker already speaks. Reading them beats guessing at graph shapes, because
//! it stops guessing entirely — whatever a stranger is running, if they can
//! render it in their own browser, this can serve it.
//!
//! The two formats differ in one way that matters. The UI keeps the graph as
//! the editor drew it: nodes with a list of `widgets_values` in screen order,
//! and links held in a separate table. `/prompt` wants each node's inputs
//! named. Converting means walking each node's declared inputs and taking
//! either the other end of a link or the next widget value.
//!
//! Anything ambiguous is refused rather than guessed. A graph converted wrong
//! renders *something*, and something wrong is worse than a clear refusal —
//! the operator has a workflow they can fix, and the model simply is not
//! advertised until they do.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The values ComfyUI appends after a seed widget. They are the editor's
/// "what to do next time" control, not an input the graph has, so they sit in
/// `widgets_values` with nothing to map onto.
const AFTER_GENERATE: [&str; 4] = ["fixed", "increment", "decrement", "randomize"];

/// A node the editor draws but the backend does not run.
const PASSTHROUGH: [&str; 3] = ["Reroute", "PrimitiveNode", "Note"];

#[derive(Debug)]
pub struct Converted {
    /// The graph, ready to POST to `/prompt`.
    pub graph: Value,
    /// The model this workflow loads, if it names one — which is what lets a
    /// saved workflow be matched to a model a client asks for, with no
    /// configuration anywhere.
    pub model: Option<String>,
    /// Where the prompt and the seed live in *this* graph, e.g.
    /// `{"prompt": "5.inputs.text", "seed": "198.inputs.seed"}`.
    pub slots: BTreeMap<String, String>,
}

/// Convert one saved workflow. `object_info` is ComfyUI's own node catalogue.
pub fn convert(ui: &Value, object_info: &Value) -> Result<Converted, String> {
    let nodes = ui
        .get("nodes")
        .and_then(|n| n.as_array())
        .ok_or("not a saved workflow: no nodes")?;

    // link id -> (origin node, origin slot)
    let mut links: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    for link in ui.get("links").and_then(|l| l.as_array()).unwrap_or(&vec![]) {
        // [id, origin_node, origin_slot, target_node, target_slot, type]
        let Some(fields) = link.as_array() else { continue };
        if fields.len() < 3 {
            continue;
        }
        let (Some(id), Some(from), Some(slot)) =
            (fields[0].as_i64(), fields[1].as_i64(), fields[2].as_i64())
        else {
            continue;
        };
        links.insert(id, (from.to_string(), slot));
    }

    // Nodes the editor is hiding: muted (2) and bypassed (4) both mean "do not
    // run me". A bypassed node passes its input through, which the frontend
    // resolves; refusing is safer than reconnecting the graph ourselves.
    let mut skipped = Vec::new();
    for node in nodes {
        let mode = node.get("mode").and_then(|m| m.as_i64()).unwrap_or(0);
        if mode == 2 || mode == 4 {
            skipped.push(node.get("id").map(|i| i.to_string()).unwrap_or_default());
        }
    }
    if !skipped.is_empty() {
        return Err(format!(
            "workflow has muted or bypassed nodes ({}), which only the editor knows how to resolve",
            skipped.join(", ")
        ));
    }

    let mut graph = Map::new();
    for node in nodes {
        let id = node
            .get("id")
            .map(|i| i.to_string().trim_matches('"').to_string())
            .ok_or("a node has no id")?;
        let class = node
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| format!("node {id} has no type"))?;

        if PASSTHROUGH.contains(&class) {
            return Err(format!(
                "workflow uses {class}, which the editor resolves rather than the server"
            ));
        }

        let spec = object_info.get(class);
        if spec.is_none() {
            return Err(format!(
                "this ComfyUI does not have the node {class} that the workflow uses"
            ));
        }

        let mut inputs = Map::new();
        let widget_values = node
            .get("widgets_values")
            .and_then(|w| w.as_array())
            .cloned()
            .unwrap_or_default();
        let mut widget_at = 0usize;

        for input in node.get("inputs").and_then(|i| i.as_array()).unwrap_or(&vec![]) {
            let Some(name) = input.get("name").and_then(|n| n.as_str()) else {
                continue;
            };

            // Connected: the value is whatever the other end produces.
            if let Some(link_id) = input.get("link").and_then(|l| l.as_i64()) {
                let (from, slot) = links
                    .get(&link_id)
                    .ok_or_else(|| format!("node {id} input '{name}' has a dangling link"))?;
                inputs.insert(name.into(), Value::Array(vec![from.clone().into(), (*slot).into()]));
                continue;
            }

            // Not connected and not a widget: an optional input left empty.
            if input.get("widget").is_none() {
                continue;
            }

            let expected = required_spec(spec.unwrap(), name);
            let value = next_widget(&widget_values, &mut widget_at, expected.as_ref())
                .ok_or_else(|| format!("node {id} ({class}) has no saved value for '{name}'"))?;

            // An input this server's copy of the node does not declare. The
            // saved value still occupies a place in `widgets_values`, so it
            // has to be consumed to keep the rest aligned — but sending it
            // would be describing a node the server does not have.
            if expected.is_none() {
                tracing::debug!("{class} on this server has no input '{name}'; leaving it out");
                continue;
            }
            inputs.insert(name.into(), value);
        }

        graph.insert(
            id,
            serde_json::json!({ "class_type": class, "inputs": Value::Object(inputs) }),
        );
    }

    let graph = Value::Object(graph);
    let model = model_of(&graph);
    let slots = slots_of(&graph);
    if !slots.contains_key("prompt") {
        return Err("workflow has no text prompt this worker could fill".into());
    }
    Ok(Converted { graph, model, slots })
}

/// The catalogue entry for one input, if it is a required one.
fn required_spec(node_spec: &Value, input: &str) -> Option<Value> {
    node_spec
        .pointer(&format!("/input/required/{input}"))
        .or_else(|| node_spec.pointer(&format!("/input/optional/{input}")))
        .cloned()
}

/// Take the next saved value for a widget, stepping over the editor's extras.
///
/// `widgets_values` holds what the editor drew, which includes controls the
/// graph has no input for — the `randomize` that follows a seed being the one
/// everybody meets. Skipping it by name keeps every later widget aligned; an
/// off-by-one here silently swaps steps for cfg and renders nonsense.
fn next_widget(values: &[Value], at: &mut usize, expected: Option<&Value>) -> Option<Value> {
    while *at < values.len() {
        let candidate = &values[*at];
        *at += 1;

        if candidate
            .as_str()
            .is_some_and(|s| AFTER_GENERATE.contains(&s))
            && !accepts(expected, candidate)
        {
            continue;
        }
        return Some(candidate.clone());
    }
    None
}

/// Could this input legitimately hold this value?
///
/// Only used to tell a real choice from the editor's leftovers: a combo input
/// whose options include `"randomize"` should keep it.
fn accepts(expected: Option<&Value>, value: &Value) -> bool {
    let Some(spec) = expected.and_then(|s| s.as_array()).and_then(|a| a.first()) else {
        return false;
    };
    spec.as_array()
        .is_some_and(|options| options.contains(value))
}

/// The model a graph loads, from whichever loader it uses.
pub fn model_of(graph: &Value) -> Option<String> {
    const LOADERS: [(&str, &str); 4] = [
        ("CheckpointLoaderSimple", "ckpt_name"),
        ("CheckpointLoader", "ckpt_name"),
        ("UNETLoader", "unet_name"),
        ("UnetLoaderGGUF", "unet_name"),
    ];
    let nodes = graph.as_object()?;
    for (class, field) in LOADERS {
        for node in nodes.values() {
            if node.get("class_type").and_then(|c| c.as_str()) == Some(class) {
                if let Some(name) = node.pointer(&format!("/inputs/{field}")).and_then(|n| n.as_str())
                {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Where to write the prompt and the seed in a converted graph.
///
/// The positive prompt is the text node the sampler takes its `positive`
/// conditioning from — not simply the first `CLIPTextEncode`, because the
/// negative one is usually drawn first and filling that would invert the
/// request.
pub fn slots_of(graph: &Value) -> BTreeMap<String, String> {
    let mut slots = BTreeMap::new();
    let Some(nodes) = graph.as_object() else {
        return slots;
    };

    let sampler = nodes.iter().find(|(_, n)| {
        n.get("class_type")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains("KSampler") || c.contains("SamplerCustom"))
    });

    if let Some((sampler_id, sampler_node)) = sampler {
        for field in ["seed", "noise_seed"] {
            if sampler_node.pointer(&format!("/inputs/{field}")).is_some() {
                slots.insert("seed".into(), format!("{sampler_id}.inputs.{field}"));
                break;
            }
        }

        // Follow `positive` back to the node that produced it.
        if let Some(source) = sampler_node
            .pointer("/inputs/positive")
            .and_then(|p| p.as_array())
            .and_then(|p| p.first())
            .and_then(|id| id.as_str())
        {
            if nodes
                .get(source)
                .and_then(|n| n.pointer("/inputs/text"))
                .is_some()
            {
                slots.insert("prompt".into(), format!("{source}.inputs.text"));
            }
        }
    }

    // No sampler shape we recognise: any single text input is still better
    // than refusing, but two are ambiguous and left alone.
    if !slots.contains_key("prompt") {
        let texts: Vec<&String> = nodes
            .iter()
            .filter(|(_, n)| n.pointer("/inputs/text").is_some_and(|t| t.is_string()))
            .map(|(id, _)| id)
            .collect();
        if texts.len() == 1 {
            slots.insert("prompt".into(), format!("{}.inputs.text", texts[0]));
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workflow saved by a real ComfyUI, and that server's own node
    /// catalogue. Fixtures rather than hand-written JSON: the shape of this
    /// format is decided by ComfyUI, and a test written from memory would
    /// pass against a format that does not exist.
    fn fixtures() -> (Value, Value) {
        (
            serde_json::from_str(include_str!("../../tests/fixtures/ui_workflow.json")).unwrap(),
            serde_json::from_str(include_str!("../../tests/fixtures/object_info.json")).unwrap(),
        )
    }

    #[test]
    fn a_workflow_saved_in_the_editor_becomes_one_the_server_can_run() {
        let (ui, info) = fixtures();
        let out = convert(&ui, &info).expect("converts");

        // Every node is present, keyed by id, in /prompt's shape.
        let graph = out.graph.as_object().unwrap();
        assert_eq!(graph.len(), 7);
        assert_eq!(graph["198"]["class_type"], "KSampler");

        // Links became references to the producing node and slot.
        assert_eq!(graph["198"]["inputs"]["model"], serde_json::json!(["201", 0]));
        assert_eq!(graph["199"]["inputs"]["samples"], serde_json::json!(["198", 0]));

        // Widgets became named inputs — and the editor's `randomize`, which
        // follows the seed and has no input to land on, did not shift them.
        assert_eq!(graph["198"]["inputs"]["seed"], 794205170038274i64);
        assert_eq!(graph["198"]["inputs"]["steps"], 28);
        assert_eq!(graph["198"]["inputs"]["cfg"], 3.5);
        assert_eq!(graph["198"]["inputs"]["sampler_name"], "dpmpp_2m_sde");
        assert_eq!(graph["198"]["inputs"]["scheduler"], "karras");
        assert_eq!(graph["198"]["inputs"]["denoise"], 1);
    }

    #[test]
    fn the_workflow_says_which_model_it_serves() {
        let (ui, info) = fixtures();
        let out = convert(&ui, &info).unwrap();
        // This is what makes it zero-configuration: the graph names its own
        // checkpoint, so it can be offered for that model without anybody
        // writing a mapping.
        assert_eq!(out.model.as_deref(), Some("lustifyNSFWCheckpoint_ggwpV7.safetensors"));
    }

    #[test]
    fn the_prompt_slot_is_the_one_the_sampler_calls_positive() {
        let (ui, info) = fixtures();
        let out = convert(&ui, &info).unwrap();

        let prompt = out.slots.get("prompt").expect("a prompt slot");
        assert_eq!(out.slots.get("seed").map(String::as_str), Some("198.inputs.seed"));

        // Two CLIPTextEncode nodes here. Taking the first would fill the
        // negative prompt and invert what was asked for.
        let positive = out.graph["198"]["inputs"]["positive"][0].as_str().unwrap();
        assert_eq!(prompt, &format!("{positive}.inputs.text"));
    }

    #[test]
    fn a_workflow_the_server_could_not_run_is_refused_rather_than_guessed_at() {
        let (ui, info) = fixtures();

        // A node this ComfyUI does not have.
        let mut unknown = ui.clone();
        unknown["nodes"][0]["type"] = "SomeCustomNodeNobodyInstalled".into();
        assert!(convert(&unknown, &info).unwrap_err().contains("does not have the node"));

        // Muted or bypassed nodes: the editor resolves those, the server does
        // not, and reconnecting the graph ourselves would be inventing one.
        let mut muted = ui.clone();
        muted["nodes"][0]["mode"] = 4.into();
        assert!(convert(&muted, &info).unwrap_err().contains("bypassed"));

        // Nothing to write a prompt into.
        let mut promptless = ui.clone();
        for node in promptless["nodes"].as_array_mut().unwrap() {
            if node["type"] == "CLIPTextEncode" {
                node["type"] = "VAEDecode".into();
            }
        }
        assert!(convert(&promptless, &info).is_err());
    }
}

/// What a workflow says a family of models wants.
///
/// Mined from ComfyUI's own templates — the ones custom-node packs ship and
/// the editor offers under "Browse templates". They are the author's own
/// answer to "how do you run this model", which beats anything this worker
/// could infer from a filename: the latent node, the VAE and the sampler
/// settings are all in there, and getting any of them wrong produces a picture
/// that renders successfully and looks like a film negative.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    /// The `CLIPLoader` type this pipeline loads its encoder as, e.g. `krea2`.
    pub family: Option<String>,
    /// The model the template was written for, for matching by name.
    pub model: Option<String>,
    /// The VAE it decodes with. Wrong VAE, wrong colours.
    pub vae: Option<String>,
    /// `EmptyLatentImage` for four-channel models, `EmptySD3LatentImage` for
    /// sixteen-channel ones. Wrong latent, pixel soup.
    pub latent: String,
    pub steps: Option<i64>,
    pub cfg: Option<f64>,
    pub sampler: Option<String>,
    pub scheduler: Option<String>,
}

impl Default for Recipe {
    fn default() -> Self {
        Self {
            family: None,
            model: None,
            vae: None,
            latent: "EmptyLatentImage".into(),
            steps: None,
            cfg: None,
            sampler: None,
            scheduler: None,
        }
    }
}

/// Read the recipe out of a template, in either format.
///
/// Positional widget reading is fine here, unlike a full conversion: these are
/// a handful of well-known nodes whose widget order is fixed by ComfyUI
/// itself, and a value that is missing or surprising simply leaves that part
/// of the recipe unset.
pub fn recipe_of(ui: &Value) -> Recipe {
    let mut recipe = Recipe::default();
    let Some(nodes) = ui.get("nodes").and_then(|n| n.as_array()) else {
        return recipe;
    };

    for node in nodes {
        let class = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let widgets = node
            .get("widgets_values")
            .and_then(|w| w.as_array())
            .cloned()
            .unwrap_or_default();
        let text = |i: usize| widgets.get(i).and_then(|v| v.as_str()).map(str::to_string);

        match class {
            "UNETLoader" | "CheckpointLoaderSimple" | "UnetLoaderGGUF" => {
                recipe.model = recipe.model.take().or_else(|| text(0));
            }
            "CLIPLoader" => {
                recipe.family = recipe.family.take().or_else(|| text(1));
            }
            "DualCLIPLoader" => {
                recipe.family = recipe.family.take().or_else(|| text(2));
            }
            "VAELoader" => recipe.vae = recipe.vae.take().or_else(|| text(0)),
            // Any latent-shaped source: the class name is the part that
            // matters, because it decides how many channels the noise has.
            c if c.starts_with("Empty") && c.contains("Latent") => {
                recipe.latent = c.to_string();
            }
            "KSampler" => {
                // [seed, control_after_generate, steps, cfg, sampler, scheduler, denoise]
                recipe.steps = widgets.get(2).and_then(|v| v.as_i64());
                recipe.cfg = widgets.get(3).and_then(|v| v.as_f64());
                recipe.sampler = text(4);
                recipe.scheduler = text(5);
            }
            _ => {}
        }
    }
    recipe
}

#[cfg(test)]
mod recipe_tests {
    use super::*;

    #[test]
    fn a_template_tells_us_how_its_model_is_meant_to_be_run() {
        let template: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/template_krea2.json")).unwrap();
        let recipe = recipe_of(&template);

        // The two that made pictures come back looking like film negatives:
        // this family decodes with the Qwen VAE, not Flux's `ae`, and samples
        // from sixteen-channel latents, not four.
        assert_eq!(recipe.vae.as_deref(), Some("qwen_image_vae.safetensors"));
        assert_eq!(recipe.latent, "EmptySD3LatentImage");

        assert_eq!(recipe.family.as_deref(), Some("krea2"));
        assert_eq!(recipe.steps, Some(10));
        assert_eq!(recipe.cfg, Some(1.0));
        assert_eq!(recipe.sampler.as_deref(), Some("euler"));
        assert_eq!(recipe.scheduler.as_deref(), Some("simple"));
        assert!(recipe.model.as_deref().is_some_and(|m| m.contains("krea2")));
    }

    #[test]
    fn a_template_with_nothing_to_say_leaves_the_ordinary_defaults() {
        let recipe = recipe_of(&serde_json::json!({ "nodes": [] }));
        assert_eq!(recipe.latent, "EmptyLatentImage", "four channels, as before");
        assert_eq!(recipe.vae, None);
        assert_eq!(recipe.steps, None);
    }
}
