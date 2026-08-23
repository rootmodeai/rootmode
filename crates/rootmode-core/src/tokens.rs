//! OpenAI tokenizer — the count we bill against.
//!
//! A worker reports its own usage. That is fine when the worker is ours and
//! the number came from the inference server. It is not fine when the worker
//! is someone else's, or when it is one of ours forwarding to OpenRouter:
//! under-counting tokens is how we pay for work we did not charge for.
//!
//! So we count ourselves, with the same BPE OpenAI uses (`o200k_base` for
//! current models, `cl100k_base` for GPT-4 / 3.5). Provider-reported usage
//! is kept only when it is *higher* — a cache hit is the one number we
//! cannot observe locally, and is taken from the provider as a subset of
//! prompt tokens, never invented.

use tiktoken_rs::tokenizer::{get_tokenizer, Tokenizer};
use tiktoken_rs::{cl100k_base_singleton, o200k_base_singleton, CoreBPE};

use crate::job::{LlmParams, ToolCall};

/// High-detail 1024×1024 OpenAI vision tiles: 85 + 170×4.
///
/// We cannot see the real dimensions of a base64 picture cheaply, and
/// under-counting images is a way to lose money, so this is the conservative
/// default rather than the 85-token "low detail" figure.
pub const IMAGE_TOKENS: u64 = 765;

/// Tokens of framing around each chat message (`<|im_start|>role`).
const TOKENS_PER_MESSAGE: u64 = 3;
/// Priming the assistant reply (`<|start|>assistant<|message|>`).
const REPLY_PRIMING: u64 = 3;
/// Per tool / function call on top of its name and arguments.
const TOOL_CALL_OVERHEAD: u64 = 1;
/// Preamble OpenAI adds when tools are in the request.
const TOOLS_PREAMBLE: u64 = 9;

/// Prompt, completion, and the two subsets that change the price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Tokens the model read, including any that were served from cache.
    pub prompt: u64,
    /// Tokens the model wrote, including reasoning.
    pub completion: u64,
    /// Subset of [`Self::prompt`] that hit a provider cache. Only a provider
    /// knows this; we never invent it.
    pub cached: u64,
    /// Subset of [`Self::completion`] spent thinking rather than answering.
    pub reasoning: u64,
}

impl TokenUsage {
    pub fn total(self) -> u64 {
        self.prompt.saturating_add(self.completion)
    }

    pub fn uncached_prompt(self) -> u64 {
        self.prompt.saturating_sub(self.cached)
    }

    pub fn is_zero(self) -> bool {
        self.prompt == 0 && self.completion == 0
    }

    /// What a provider claimed. `None` when it claimed nothing.
    pub fn from_meta(meta: &serde_json::Value) -> Option<Self> {
        let prompt = as_u64(meta.get("prompt_tokens"));
        let completion = as_u64(meta.get("completion_tokens"));
        if prompt == 0 && completion == 0 {
            return None;
        }
        let cached = as_u64(meta.get("cached_tokens")).min(prompt);
        let reasoning = as_u64(meta.get("reasoning_tokens")).min(completion);
        Some(Self {
            prompt,
            completion,
            cached,
            reasoning,
        })
    }

    /// Merge a local count with whatever the provider reported.
    ///
    /// Prompt and completion take the **max**, so an under-reporting worker
    /// or a silent OpenRouter stream cannot shrink the bill below what we
    /// can count. Cached tokens are taken only from the provider — we cannot
    /// see a cache hit — and are clamped to the billed prompt total so they
    /// never exceed it.
    pub fn reconcile(self, reported: Option<Self>) -> Self {
        let Some(theirs) = reported.filter(|r| !r.is_zero()) else {
            return self;
        };
        let prompt = self.prompt.max(theirs.prompt);
        let completion = self.completion.max(theirs.completion);
        Self {
            prompt,
            completion,
            cached: theirs.cached.min(prompt),
            reasoning: self.reasoning.max(theirs.reasoning).min(completion),
        }
    }

    /// Write the billed counts into a result's `meta`.
    pub fn write_into(&self, meta: &mut serde_json::Value) {
        let Some(obj) = meta.as_object_mut() else {
            return;
        };
        obj.insert("prompt_tokens".into(), self.prompt.into());
        obj.insert("completion_tokens".into(), self.completion.into());
        obj.insert("cached_tokens".into(), self.cached.into());
        obj.insert("reasoning_tokens".into(), self.reasoning.into());
        obj.insert("total_tokens".into(), self.total().into());
        obj.insert("tokens_measured".into(), true.into());
        obj.insert("tokenizer".into(), "openai".into());
    }

    /// Count a finished chat completion the way OpenAI bills one.
    pub fn measure(
        params: &LlmParams,
        text: Option<&str>,
        thinking: Option<&str>,
        tool_calls: &[ToolCall],
    ) -> Self {
        let bpe = encoding_for(params.model_id.as_deref());
        let prompt = count_prompt(bpe, params);
        let reasoning = thinking.map(|t| count_text(bpe, t)).unwrap_or(0);
        let mut completion = reasoning;
        if let Some(text) = text {
            completion += count_text(bpe, text);
        }
        for call in tool_calls {
            completion += count_text(bpe, &call.id);
            completion += count_text(bpe, &call.name);
            completion += count_text(bpe, &call.arguments);
            completion += TOOL_CALL_OVERHEAD;
        }
        Self {
            prompt,
            completion,
            cached: 0,
            reasoning,
        }
    }
}

fn as_u64(v: Option<&serde_json::Value>) -> u64 {
    v.and_then(serde_json::Value::as_u64).unwrap_or(0)
}

/// The BPE OpenAI would use for this model name, defaulting to `o200k_base`.
pub fn encoding_for(model: Option<&str>) -> &'static CoreBPE {
    let Some(model) = model else {
        return o200k_base_singleton();
    };
    // Catalogue keys (`openai/gpt-4o`, `openai/gpt-4o-mini`) and the bare
    // name both have to land on the same encoding.
    let short = model.rsplit('/').next().unwrap_or(model);
    match get_tokenizer(short).or_else(|| get_tokenizer(model)) {
        Some(Tokenizer::Cl100kBase) => cl100k_base_singleton(),
        _ => o200k_base_singleton(),
    }
}

pub fn count_text(bpe: &CoreBPE, text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    bpe.count_ordinary(text) as u64
}

fn count_prompt(bpe: &CoreBPE, params: &LlmParams) -> u64 {
    let mut n = REPLY_PRIMING;
    for m in &params.messages {
        n += TOKENS_PER_MESSAGE;
        n += count_text(bpe, &m.role);
        n += count_text(bpe, &m.content);
        if let Some(id) = &m.tool_call_id {
            n += count_text(bpe, id);
        }
        for call in &m.tool_calls {
            n += count_text(bpe, &call.id);
            n += count_text(bpe, &call.name);
            n += count_text(bpe, &call.arguments);
            n += TOOL_CALL_OVERHEAD;
        }
        n += m.images.len() as u64 * IMAGE_TOKENS;
    }
    if !params.tools.is_empty() {
        n += TOOLS_PREAMBLE;
        for t in &params.tools {
            n += count_text(bpe, &t.name);
            if let Some(d) = &t.description {
                n += count_text(bpe, d);
            }
            n += count_text(bpe, &t.input_schema.to_string());
            n += 3;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{ChatMessage, ToolDef};

    fn ping() -> LlmParams {
        LlmParams {
            model_hash: None,
            model_id: Some("gpt-4o".into()),
            messages: vec![ChatMessage::new("user", "ping")],
            tools: Vec::new(),
            max_tokens: 16,
            temperature: 0.0,
        }
    }

    #[test]
    fn o200k_counts_a_known_string() {
        // Locked against tiktoken's own count so a crate bump that silently
        // changes the encoding cannot ship.
        let bpe = o200k_base_singleton();
        assert_eq!(count_text(bpe, "hello world"), 2);
        assert_eq!(count_text(bpe, ""), 0);
    }

    #[test]
    fn gpt4_family_uses_cl100k_and_gpt4o_uses_o200k() {
        // Different vocabularies: a billing path that always used one of
        // them would be wrong for the other.
        assert!(std::ptr::eq(
            encoding_for(Some("gpt-4")),
            cl100k_base_singleton()
        ));
        assert!(std::ptr::eq(
            encoding_for(Some("gpt-3.5-turbo")),
            cl100k_base_singleton()
        ));
        assert!(std::ptr::eq(
            encoding_for(Some("gpt-4o")),
            o200k_base_singleton()
        ));
        assert!(std::ptr::eq(
            encoding_for(Some("openai/gpt-4o-mini")),
            o200k_base_singleton()
        ));
        // Anything we do not recognise — Llama on OpenRouter, a local
        // checkpoint — still gets an OpenAI tokenizer, not a guess of zero.
        assert!(std::ptr::eq(
            encoding_for(Some("llama-3.3-70b-instruct")),
            o200k_base_singleton()
        ));
    }

    #[test]
    fn a_chat_turn_includes_framing_not_just_the_words() {
        let usage = TokenUsage::measure(&ping(), Some("pong"), None, &[]);
        // "ping" is 1 token; the rest is role + per-message overhead + reply
        // priming. A count equal to the word count would be the naive
        // split-on-whitespace guess this exists to replace.
        assert!(
            usage.prompt > count_text(o200k_base_singleton(), "ping"),
            "prompt was {}",
            usage.prompt
        );
        assert_eq!(usage.completion, count_text(o200k_base_singleton(), "pong"));
        assert_eq!(usage.cached, 0);
        assert_eq!(usage.reasoning, 0);
    }

    #[test]
    fn reasoning_is_billed_as_completion() {
        let usage = TokenUsage::measure(&ping(), Some("42"), Some("let me think"), &[]);
        let bpe = o200k_base_singleton();
        let thought = count_text(bpe, "let me think");
        let answer = count_text(bpe, "42");
        assert_eq!(usage.reasoning, thought);
        assert_eq!(usage.completion, thought + answer);
    }

    #[test]
    fn pictures_in_a_message_are_not_free() {
        let mut params = ping();
        params.messages[0] = ChatMessage::new("user", "what is this").with_images(vec![
            "iVBORw0KGgo=".into(),
        ]);
        let usage = TokenUsage::measure(&params, Some("a cat"), None, &[]);
        assert!(
            usage.prompt >= IMAGE_TOKENS,
            "prompt was {}",
            usage.prompt
        );
    }

    #[test]
    fn tools_add_to_the_prompt() {
        let mut params = ping();
        params.tools.push(ToolDef {
            name: "lookup".into(),
            description: Some("find a thing".into()),
            input_schema: serde_json::json!({"type": "object"}),
        });
        let with_tools = TokenUsage::measure(&params, Some("ok"), None, &[]);
        let without = TokenUsage::measure(&ping(), Some("ok"), None, &[]);
        assert!(
            with_tools.prompt > without.prompt,
            "with {} without {}",
            with_tools.prompt,
            without.prompt
        );
    }

    #[test]
    fn reconcile_takes_the_max_so_an_under_count_cannot_shrink_the_bill() {
        let ours = TokenUsage {
            prompt: 100,
            completion: 40,
            cached: 0,
            reasoning: 10,
        };
        let theirs = TokenUsage {
            prompt: 80,
            completion: 55,
            cached: 60,
            reasoning: 5,
        };
        let billed = ours.reconcile(Some(theirs));
        assert_eq!(billed.prompt, 100, "ours was higher");
        assert_eq!(billed.completion, 55, "theirs was higher");
        assert_eq!(billed.cached, 60, "cache is the provider's to report");
        assert_eq!(billed.reasoning, 10);
        assert_eq!(billed.uncached_prompt(), 40);
    }

    #[test]
    fn cache_is_never_invented() {
        let ours = TokenUsage {
            prompt: 100,
            completion: 10,
            cached: 0,
            reasoning: 0,
        };
        assert_eq!(ours.reconcile(None).cached, 0);
        assert_eq!(ours.reconcile(Some(TokenUsage::default())).cached, 0);
        // A provider that claims more cache than prompt is clamped, not
        // trusted: that would make uncached prompt wrap and under-bill.
        let inflated = TokenUsage {
            prompt: 10,
            completion: 1,
            cached: 999,
            reasoning: 0,
        };
        let billed = ours.reconcile(Some(inflated));
        assert_eq!(billed.cached, billed.prompt.min(999));
        assert!(billed.cached <= billed.prompt);
    }

    #[test]
    fn from_meta_ignores_an_empty_or_guessed_count() {
        assert_eq!(TokenUsage::from_meta(&serde_json::json!({})), None);
        assert_eq!(
            TokenUsage::from_meta(&serde_json::json!({
                "prompt_tokens": null, "completion_tokens": 2, "tokens_measured": false
            })),
            Some(TokenUsage {
                prompt: 0,
                completion: 2,
                cached: 0,
                reasoning: 0
            })
        );
        assert_eq!(
            TokenUsage::from_meta(&serde_json::json!({
                "prompt_tokens": 12,
                "completion_tokens": 30,
                "cached_tokens": 4,
                "reasoning_tokens": 8
            })),
            Some(TokenUsage {
                prompt: 12,
                completion: 30,
                cached: 4,
                reasoning: 8
            })
        );
    }
}
