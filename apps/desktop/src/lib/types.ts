// Mirrors the serde shapes in crates/rootmode-core and src-tauri/src/store.rs.
// Protocol v1 — see docs/PROTOCOL.md.

export const PROTOCOL_VERSION = 1;

export type JobKind = "llm" | "image" | "video";
export type JobStatus = "queued" | "running" | "done" | "failed";
export type PeerStatus = "online" | "offline" | "unknown" | "mismatch";

export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

export interface LlmParams {
  kind: "llm";
  model_hash?: string;
  model_id?: string;
  messages: ChatMessage[];
  max_tokens: number;
  temperature: number;
}

/**
 * A model, and words. Nothing else.
 *
 * Sampler steps, guidance, size and the shape of the graph are how an operator
 * built their pipeline; a client cannot know the right values for a checkpoint
 * it has never seen. The worker reports what it used in the result's `meta`.
 */
export interface ImageParams {
  kind: "image";
  model_hash?: string;
  checkpoint_id?: string;
  prompt: string;
  /** A picture to start from, base64 — "more like this, but…". */
  from_image?: string;
  /** How much to change it, 0–1. Absent lets the provider choose. */
  change?: number;
}

/** A clip from words. Length, fps and the graph stay with the worker. */
export interface VideoParams {
  kind: "video";
  model_hash?: string;
  checkpoint_id?: string;
  prompt: string;
  /** First frame, base64, for image-to-video. Absent is text-to-video. */
  from_image?: string;
  /** How long, in seconds. Absent: the provider's default clip. */
  seconds?: number;
  /** "720p", "1080p", "4K" — from the provider's menu. */
  resolution?: string;
  /** "16:9", "9:16" — from the provider's menu. */
  aspect_ratio?: string;
  /** Sound, on providers that sell it apart. */
  audio?: boolean;
}

export type JobPayload = LlmParams | ImageParams | VideoParams;

export type AudioOffer = "never" | "always" | "optional";

/** USD per second, after markup, for one shape of clip. */
export interface VideoRate {
  resolution?: string | null;
  audio: boolean;
  from_image: boolean;
  usd_per_second: number;
  minimum_usd: number;
}

/** A video model's menu: what may be chosen, the defaults, and the rates. */
export interface VideoOffer {
  durations: number[];
  default_seconds: number;
  resolutions: string[];
  default_resolution?: string | null;
  aspect_ratios: string[];
  default_aspect?: string | null;
  audio: AudioOffer;
  /** Whether a first frame is taken at all. */
  first_frame: boolean;
  rates: VideoRate[];
}

export interface Price {
  amount: number;
  currency: string;
  /** USD / million uncached prompt tokens, when the worker splits rates. */
  input?: number;
  /** USD / million completion tokens. */
  output?: number;
  /** USD / million cached prompt tokens. */
  cache?: number;
  cache_write?: number;
}

export interface ModelDescriptor {
  id: string;
  sha256?: string;
  kind: JobKind;
  price?: Price;
}

/** A model you can ask for, and the provider that would serve it. */
export interface ModelOption {
  model: string;
  kind: JobKind;
  providers: number;
  peer_id: string;
  peer_label: string;
  /** Per million tokens for text, per image for images. 0 = free. */
  price: number;
  currency: string;
  latency_ms: number | null;
  unpriced: boolean;
}

export interface Peer {
  id: string;
  label: string;
  endpoint: string;
  public_key: string | null;
  peer_id: string | null;
  status: PeerStatus;
  latency_ms: number | null;
  caps: string[];
  models: ModelDescriptor[];
  max_concurrent: number;
  last_seen: number | null;
  last_error: string | null;
  /** "manual" = you typed it. "discovered" = found on the network. */
  source: "manual" | "discovered";
  /**
   * ISO 3166-1 alpha-2, as the operator declared it — never geolocated, so
   * null means "did not say" rather than "unknown location".
   */
  country: string | null;
  payout: string | null;
  added_at: number;
}

export interface JobRecord {
  /// Set when the job answers a chat.
  conversation_id: string | null;
  job_id: string;
  peer_id: string;
  peer_label: string;
  kind: JobKind;
  payload: JobPayload;
  summary: string;
  model: string;
  status: JobStatus;
  progress: number;
  error: string | null;
  created_at: number;
  updated_at: number;
}

export interface ResultRecord {
  job_id: string;
  kind: JobKind;
  sha256: string;
  text: string | null;
  image_path: string | null;
  meta: Record<string, unknown> | null;
  created_at: number;
}

export interface PublicIdentity {
  peer_id: string;
  public_key_hex: string;
}

export interface Settings {
  download_dir: string;
  default_peer: string | null;
  default_llm_model: string | null;
  default_image_model: string | null;
  theme: string;
  sign_jobs: boolean;
  bootstrap: string;
  discovery: boolean;
  mock_worker: boolean;
  entry_points: number;
  gateway: boolean;
  gateway_port: number;
  heartbeat: boolean;
  intro_seen: boolean;
  app_data_dir: string;
  db_path: string;
  key_path: string;
}

/** Where other apps on this machine connect, and with what token. */
export interface GatewayStatus {
  enabled: boolean;
  running: boolean;
  port: number;
  base_url: string;
  token: string;
  substitute: boolean;
  /// The model outside tools are told to use; null means whichever is cheapest.
  model: string | null;
  error: string | null;
  requests: number;
}

/** One (model, provider) pair on offer — what the picker lists. */
export interface ProviderOption {
  model: string;
  kind: JobKind;
  peer_id: string;
  peer_label: string;
  /** Declared by the operator, never geolocated. Null = did not say. */
  peer_country: string | null;
  price: number;
  currency: string;
  unpriced: boolean;
  latency_ms: number | null;
  /** Video models: the shapes on offer and their rates. */
  video?: VideoOffer | null;
  /**
   * Set by the picker: true when the user chose this exact provider, false
   * (or absent) when they chose the model and any provider at its best
   * price will do. Never sent to the backend.
   */
  pinned?: boolean;
}

export interface DashboardStats {
  peers: number;
  peers_online: number;
  open_jobs: number;
  results: number;
  peer_id: string;
  protocol_version: number;
  discovery: boolean;
}

export interface Conversation {
  id: string;
  title: string;
  kind: JobKind;
  created_at: number;
  updated_at: number;
  preview: string;
  message_count: number;
}

export interface Message {
  id: number;
  conversation_id: string;
  role: "user" | "assistant" | "system";
  content: string;
  job_id: string | null;
  sha256: string | null;
  model: string | null;
  peer: string | null;
  /** Tokens the provider reported, when it reported any. */
  tokens: number | null;
  /**
   * What this reply was billed, in millionths of a USDC. 0 is a priced job
   * that billed nothing; null is a free provider or an unrecorded bill.
   */
  cost_micros: number | null;
  /** What the model said to itself before answering, when it said anything. */
  thinking: string | null;
  created_at: number;
}

/** Incremental tokens for a job still running. */
export interface JobDelta {
  v: number;
  job_id: string;
  text?: string;
  thinking?: string;
}

/** What the app needs to know to decide if you can do anything yet. */
export interface NetworkStatus {
  online: number;
  llm_peers: number;
  image_peers: number;
  video_peers: number;
  models: string[];
  image_models: string[];
  video_models: string[];
  searching: boolean;
}

export const MOCK_ENDPOINT = "mock://local";

/** A document dropped on the window and read by the backend. */
export interface Attachment {
  name: string;
  text: string;
  chars: number;
  truncated: boolean;
  kind: string;
}

/** A picture that was dropped in and kept; `id` is its content hash. */
export interface Picture {
  id: string;
  name: string;
  mime: string;
  bytes: number;
}

export interface DropOutcome {
  attached: Attachment[];
  /** Pictures kept for a flow to point at. */
  pictures: Picture[];
  /** One line per file that could not be read, already phrased for a person. */
  rejected: string[];
  /** Where on the page the files landed, in CSS pixels. */
  at?: { x: number; y: number } | null;
}

/** A CLI tool or editor that can be config-patched to use the local endpoint. */
export interface ToolStatus {
  key: string;
  display_name: string;
  method: string;
  installed: boolean;
  connected: boolean;
  config_path: string;
}

export interface UpdateInfo {
  current: string;
  latest: string | null;
  available: boolean;
  url: string;
}

export interface PotStatus {
  configured: boolean;
  reachable: boolean;
  client: string | null;
  app_key: string;
  balance_micros: number;
  max_per_job_micros: number;
  max_per_day_micros: number;
  spent_today_micros: number;
  reserved_micros: number;
  rpc: string;
  pot: string;
  usdc: string;
  chain_id: number;
}

export interface Deposit {
  tx_hash: string;
  amount_micros: number;
  max_per_job_micros: number;
  max_per_day_micros: number;
  block: number;
  at: number;
  url: string | null;
}

export interface ModelUsage {
  model: string;
  tokens: number;
  replies: number;
  /** Sum of the recorded bills for this model, in millionths of a USDC. */
  cost_micros: number;
}

/** One priced reply and what it was billed — a row in the spend ledger. */
export interface SpendEntry {
  job_id: string | null;
  model: string;
  peer: string | null;
  tokens: number | null;
  cost_micros: number;
  at: number;
  /** The signed ticket this charge rode on, and its payout channel. */
  cumulative_micros: number | null;
  payout: string | null;
  /** The reply ended without a bill and the worker kept the prepaid chunk. */
  abandoned: boolean;
  bond_cumulative: number | null;
  chunk_micros: number | null;
  /**
   * The on-chain transaction that collected this charge. Null means charged
   * but not yet collected (settles are batched), or the reply predates
   * settlement tracking.
   */
  settle_tx: string | null;
  settle_block: number | null;
  settle_url: string | null;
}

export type FundingKind = "ok" | "cap" | "empty" | "chain";

export interface PotCheck {
  ready: boolean;
  needs_fund: boolean;
  reason: string;
  kind: FundingKind;
  cap_micros: number;
}
