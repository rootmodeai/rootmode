// Typed wrappers over the Rust commands. This is the only place the frontend
// talks to the backend; there is no other privileged surface.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  Conversation,
  DashboardStats,
  Message,
  ModelOption,
  ProviderOption,
  NetworkStatus,
  JobKind,
  JobPayload,
  JobDelta,
  JobRecord,
  Peer,
  PublicIdentity,
  ResultRecord,
  GatewayStatus,
  DropOutcome,
  Settings,
  ToolStatus,
  PotStatus,
  PotCheck,
  Deposit,
  ModelUsage,
  SpendEntry,
  UpdateInfo,
} from "./types";

/** Backend errors arrive as strings; keep them intact for the UI to show. */
export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e);
}

export const api = {
  // identity
  getIdentity: () => invoke<PublicIdentity>("get_identity"),
  exportIdentitySecret: () => invoke<string>("export_identity_secret"),
  importIdentity: (secretHex: string) =>
    invoke<PublicIdentity>("import_identity", { secretHex }),
  regenerateIdentity: () => invoke<PublicIdentity>("regenerate_identity"),

  // peers
  listPeers: () => invoke<Peer[]>("list_peers"),
  addPeer: (label: string, endpoint: string, publicKey?: string) =>
    invoke<Peer>("add_peer", { label, endpoint, publicKey: publicKey || null }),
  removePeer: (id: string) => invoke<void>("remove_peer", { id }),
  probePeer: (id: string) => invoke<Peer>("probe_peer", { id }),
  probeAllPeers: () => invoke<Peer[]>("probe_all_peers"),
  discoverPeers: () => invoke<Peer[]>("discover_peers"),

  // jobs
  submitJob: (peerId: string, payload: JobPayload, conversationId?: string) =>
    invoke<JobRecord>("submit_job", { peerId, payload, conversationId: conversationId ?? null }),
  stopJob: (jobId: string) => invoke<void>("stop_job", { jobId }),
  listJobs: (limit?: number) => invoke<JobRecord[]>("list_jobs", { limit }),
  getJob: (jobId: string) => invoke<JobRecord | null>("get_job", { jobId }),

  // results
  getResult: (jobId: string) => invoke<ResultRecord | null>("get_result", { jobId }),
  listResults: (kind?: JobKind, limit?: number) =>
    invoke<ResultRecord[]>("list_results", { kind: kind ?? null, limit }),
  readResultImage: (jobId: string) => invoke<string>("read_result_image", { jobId }),
  /// The raw base64 of a result, for sending back as a starting point.
  readResultBytes: (jobId: string) => invoke<string>("read_result_bytes", { jobId }),
  revealResult: (jobId: string) => invoke<void>("reveal_result", { jobId }),
  /// Erase a result: the bytes on disk first, then the rows.
  deleteResult: (jobId: string) => invoke<void>("delete_result", { jobId }),

  // local endpoint
  gatewayStatus: () => invoke<GatewayStatus>("gateway_status"),
  rotateGatewayToken: () => invoke<GatewayStatus>("rotate_gateway_token"),

  // connected apps — config-patching CLI tools to use the local endpoint
  listConnectedTools: () => invoke<ToolStatus[]>("list_connected_tools"),
  connectTool: (key: string) => invoke<ToolStatus>("connect_tool", { key }),
  disconnectTool: (key: string) => invoke<ToolStatus>("disconnect_tool", { key }),
  launchTool: (key: string) => invoke<void>("launch_tool", { key }),

  // conversations
  networkStatus: () => invoke<NetworkStatus>("network_status"),
  availableModels: (kind: JobKind = "llm") =>
    invoke<ModelOption[]>("available_models", { kind }),
  /// Every provider offering every model of this kind, cheapest first.
  availableProviders: (kind: JobKind = "llm") =>
    invoke<ProviderOption[]>("available_providers", { kind }),
  listConversations: (kind?: JobKind) =>
    invoke<Conversation[]>("list_conversations", { kind: kind ?? null }),
  conversationMessages: (id: string) => invoke<Message[]>("conversation_messages", { id }),
  newConversation: (title: string, kind: JobKind) =>
    invoke<Conversation>("new_conversation", { title, kind }),
  renameConversation: (id: string, title: string) =>
    invoke<void>("rename_conversation", { id, title }),
  deleteConversation: (id: string) => invoke<void>("delete_conversation", { id }),
  /// Everything: chats, the pictures and videos they made, and the rows
  /// behind them. One call, so the wipe cannot stop half way.
  deleteAllConversations: () => invoke<void>("delete_all_conversations"),
  addMessage: (m: {
    conversationId: string;
    role: string;
    content: string;
    jobId?: string;
    sha256?: string;
    model?: string;
    peer?: string;
    tokens?: number;
  }) =>
    invoke<Message>("add_message", {
      conversationId: m.conversationId,
      role: m.role,
      content: m.content,
      jobId: m.jobId ?? null,
      sha256: m.sha256 ?? null,
      model: m.model ?? null,
      peer: m.peer ?? null,
      tokens: m.tokens ?? null,
    }),

  // settings
  getSettings: () => invoke<Settings>("get_settings"),
  setSetting: (key: string, value: string) =>
    invoke<Settings>("set_setting", { key, value }),
  dashboardStats: () => invoke<DashboardStats>("dashboard_stats"),

  potStatus: () => invoke<PotStatus>("pot_status"),
  potCheck: (price: number, unpriced: boolean, kind: JobKind) =>
    invoke<PotCheck>("pot_check", { price, unpriced, kind }),
  potOpenFund: () => invoke<string>("pot_open_fund"),
  potDeposits: () => invoke<Deposit[]>("pot_deposits"),
  tokenUsage: () => invoke<ModelUsage[]>("token_usage"),
  spendHistory: (limit?: number) =>
    invoke<SpendEntry[]>("spend_history", { limit: limit ?? null }),
  syncSettlements: () => invoke<number>("sync_settlements"),
  checkUpdate: () => invoke<UpdateInfo>("check_update"),
  /// Where this run is writing its log, for Settings to point at.
  logPath: () => invoke<string | null>("log_path"),
  skipUpdate: (version: string) => invoke<void>("skip_update", { version }),
  openUpdate: (url?: string) => invoke<void>("open_update", { url: url ?? null }),
};

export const events = {
  onJobUpdate: (cb: (job: JobRecord) => void): Promise<UnlistenFn> =>
    listen<JobRecord>("job:update", (e) => cb(e.payload)),
  onJobResult: (cb: (result: ResultRecord) => void): Promise<UnlistenFn> =>
    listen<ResultRecord>("job:result", (e) => cb(e.payload)),
  onJobDelta: (cb: (delta: JobDelta) => void): Promise<UnlistenFn> =>
    listen<JobDelta>("job:delta", (e) => cb(e.payload)),
  onPeerUpdate: (cb: (peer: Peer) => void): Promise<UnlistenFn> =>
    listen<Peer>("peer:update", (e) => cb(e.payload)),
  /// Documents were dropped on the window and read.
  onFilesDropped: (cb: (outcome: DropOutcome) => void): Promise<UnlistenFn> =>
    listen<DropOutcome>("files:dropped", (e) => cb(e.payload)),
  /// A reply was filed into a conversation by the job pipeline.
  onMessage: (cb: (message: Message) => void): Promise<UnlistenFn> =>
    listen<Message>("message:new", (e) => cb(e.payload)),
};
