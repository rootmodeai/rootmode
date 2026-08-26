import type { JobKind, ProviderOption } from "./types";

/**
 * What a model is called by the people who use it.
 *
 * The wire carries catalogue ids — `gemini-3.1-flash-image`, `veo-3.1-fast`
 * — which say nothing to someone who has heard of "Nano Banana" and "Veo".
 * This is the other half: the id stays, small, for anyone who wants it, and
 * the name people say goes first.
 *
 * Keys are the ids providers advertise (vendor prefix and `:free` already
 * dropped). Anything not listed is tidied up mechanically, and its maker
 * guessed from the family name where that is safe to do.
 */
interface Known {
  name: string;
  maker: string;
  /** Other things people call it; searchable, never shown. */
  aliases?: string[];
}

const KNOWN: Record<string, Known> = {
  // Pictures
  "gemini-3.1-flash-image": { name: "Nano Banana 2", maker: "Google", aliases: ["nano banana", "nanobanana", "gemini image"] },
  "gemini-3.1-flash-lite-image": { name: "Nano Banana 2 Lite", maker: "Google", aliases: ["nano banana", "nanobanana"] },
  "gemini-3-pro-image": { name: "Nano Banana Pro", maker: "Google", aliases: ["nano banana", "nanobanana"] },
  "gemini-2.5-flash-image": { name: "Nano Banana", maker: "Google", aliases: ["nanobanana"] },
  "gpt-5-image-mini": { name: "GPT Image Mini", maker: "OpenAI", aliases: ["dall-e", "chatgpt image"] },
  "gpt-5-image": { name: "GPT Image", maker: "OpenAI", aliases: ["dall-e", "chatgpt image"] },
  // Video
  "veo-3.1-lite": { name: "Veo 3.1 Lite", maker: "Google", aliases: ["veo"] },
  "veo-3.1-fast": { name: "Veo 3.1 Fast", maker: "Google", aliases: ["veo"] },
  "veo-3.1": { name: "Veo 3.1", maker: "Google", aliases: ["veo"] },
  "kling-v3.0-std": { name: "Kling 3.0", maker: "Kuaishou", aliases: ["kling"] },
  "kling-v3.0-pro": { name: "Kling 3.0 Pro", maker: "Kuaishou", aliases: ["kling"] },
  "seedance-2.0-fast": { name: "Seedance 2.0 Fast", maker: "ByteDance", aliases: ["seedance"] },
  "seedance-2.0": { name: "Seedance 2.0", maker: "ByteDance", aliases: ["seedance"] },
  "hailuo-2.3": { name: "Hailuo 2.3", maker: "MiniMax", aliases: ["hailuo"] },
  "wan-2.6": { name: "Wan 2.6", maker: "Alibaba", aliases: ["wan"] },
  "sora-2-pro": { name: "Sora 2 Pro", maker: "OpenAI", aliases: ["sora"] },
  "sora-2": { name: "Sora 2", maker: "OpenAI", aliases: ["sora"] },
  // Text
  "claude-opus-5": { name: "Claude Opus 5", maker: "Anthropic", aliases: ["claude"] },
  "claude-sonnet-5": { name: "Claude Sonnet 5", maker: "Anthropic", aliases: ["claude"] },
  "gpt-5.6-luna": { name: "GPT-5.6 Luna", maker: "OpenAI", aliases: ["chatgpt"] },
  "gpt-5.6-sol": { name: "GPT-5.6 Sol", maker: "OpenAI", aliases: ["chatgpt"] },
  "gemini-3.7-flash": { name: "Gemini 3.7 Flash", maker: "Google" },
  "gemini-3.6-flash": { name: "Gemini 3.6 Flash", maker: "Google" },
  "gemma-4-31b-it": { name: "Gemma 4 31B", maker: "Google" },
  "gemma-4-26b-a4b-it": { name: "Gemma 4 26B", maker: "Google" },
  "deepseek-v4-flash-0731": { name: "DeepSeek V4 Flash", maker: "DeepSeek" },
  "deepseek-v4-pro": { name: "DeepSeek V4 Pro", maker: "DeepSeek" },
  "kimi-k3": { name: "Kimi K3", maker: "Moonshot" },
  "grok-4.6": { name: "Grok 4.6", maker: "xAI" },
  "glm-5.2": { name: "GLM 5.2", maker: "Z.ai" },
  "mimo-v2.5": { name: "MiMo V2.5", maker: "Xiaomi" },
  hy3: { name: "Hunyuan 3", maker: "Tencent", aliases: ["hunyuan"] },
  "minimax-m3": { name: "MiniMax M3", maker: "MiniMax" },
  "minimax-m2.7": { name: "MiniMax M2.7", maker: "MiniMax" },
  "qwen3.8-max": { name: "Qwen 3.8 Max", maker: "Alibaba" },
  "nemotron-3-ultra-550b-a55b": { name: "Nemotron 3 Ultra", maker: "NVIDIA" },
  "nemotron-3.5-lightning": { name: "Nemotron 3.5 Lightning", maker: "NVIDIA" },
  "laguna-s-2.1": { name: "Laguna S 2.1", maker: "Poolside" },
  "laguna-xs-2.1": { name: "Laguna XS 2.1", maker: "Poolside" },
  "ox-alpha": { name: "Ox Alpha", maker: "stealth", aliases: ["0x", "ox"] },
  inkling: { name: "Inkling", maker: "Thinking Machines" },
  "north-mini-code": { name: "North Mini Code", maker: "Cohere" },
};

/** Family → maker, for ids nobody wrote down. Only families with one maker. */
const FAMILIES: [RegExp, string][] = [
  [/^(gpt|sora|o\d)\b/, "OpenAI"],
  [/^claude/, "Anthropic"],
  [/^(gemini|gemma|veo|imagen)/, "Google"],
  [/^llama/, "Meta"],
  [/^(qwen|wan)/, "Alibaba"],
  [/^deepseek/, "DeepSeek"],
  [/^(mistral|mixtral|codestral|pixtral)/, "Mistral"],
  [/^kimi/, "Moonshot"],
  [/^grok/, "xAI"],
  [/^glm/, "Z.ai"],
  [/^(minimax|hailuo)/, "MiniMax"],
  [/^kling/, "Kuaishou"],
  [/^seedance/, "ByteDance"],
  [/^nemotron/, "NVIDIA"],
  [/^(hy|hunyuan)/, "Tencent"],
  [/^mimo/, "Xiaomi"],
  [/^laguna/, "Poolside"],
  [/^phi/, "Microsoft"],
  [/^(command|north)/, "Cohere"],
];

export interface Described {
  /** What to call it. */
  name: string;
  maker: string | null;
  /** The id on the wire — shown small, so nothing is hidden. */
  id: string;
  /** True when `name` is only a tidied-up id, not a name anyone uses. */
  guessed: boolean;
}

/** `llama-3.3-70b-instruct` → `Llama 3.3 70B Instruct`. */
function tidy(id: string): string {
  return id
    .replace(/:free$/, "")
    .split(/[-_/]+/)
    .filter(Boolean)
    .map((w) => {
      if (/^\d+(\.\d+)?[bm]$/i.test(w)) return w.toUpperCase();
      if (/^(gpt|glm|llm|qwq|hy)$/i.test(w)) return w.toUpperCase();
      if (/^v?\d/.test(w)) return w.replace(/^v(\d)/, "$1");
      return w.charAt(0).toUpperCase() + w.slice(1);
    })
    .join(" ");
}

export function describe(id: string): Described {
  const bare = id.replace(/^.*\//, "").replace(/:free$/, "");
  const known = KNOWN[bare];
  if (known) return { name: known.name, maker: known.maker, id, guessed: false };
  const maker = FAMILIES.find(([re]) => re.test(bare))?.[1] ?? null;
  return { name: tidy(bare), maker, id, guessed: true };
}

/** Everything a search for this model should match on. */
export function searchTerms(id: string): string {
  const bare = id.replace(/^.*\//, "").replace(/:free$/, "");
  const d = describe(id);
  return [id, d.name, d.maker ?? "", ...(KNOWN[bare]?.aliases ?? [])].join(" ").toLowerCase();
}

/** "$0.09 / picture", "$0.35 / clip", "$1.20 / M tokens", or "free". */
export function priceLabel(o: { price: number; currency: string; unpriced: boolean; kind: JobKind }): string {
  if (o.unpriced || o.price <= 0) return "free";
  const unit = o.kind === "image" ? "picture" : o.kind === "video" ? "clip" : "M tokens";
  return `${o.price.toFixed(2)} ${o.currency} / ${unit}`;
}

/**
 * Who actually gets the job.
 *
 * A model chosen from the list is a model, not a machine: among providers
 * tied at the lowest price for it, one is drawn at random on every send, so
 * equal offers share the load instead of everyone landing on whichever
 * node sorts first. A provider the user pinned by hand is used as pinned.
 */
export function targetFor(option: ProviderOption, rows: ProviderOption[]): ProviderOption {
  if (option.pinned) return option;
  const same = rows.filter((r) => r.model === option.model);
  if (same.length === 0) return option;
  const floor = Math.min(...same.map((r) => r.price));
  const tied = same.filter((r) => r.price === floor);
  return tied[Math.floor(Math.random() * tied.length)] ?? option;
}
