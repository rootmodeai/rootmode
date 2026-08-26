import { api } from "./api";
import { targetFor } from "./models";
import type { JobKind, JobPayload, ProviderOption } from "./types";

/**
 * Flows: models wired together on a canvas, run as a chain of ordinary jobs.
 *
 * Nothing here is new to the network. Every model node becomes a job
 * submitted the same way Chat and Create submit theirs — routed to the
 * cheapest provider, locked, billed and settled like one — and a wire is
 * only "hand the previous node's result to the next node's payload". The
 * canvas is a directed graph; running it is walking that graph, starting
 * every node whose inputs are ready and waiting for the rest.
 */

/** What flows along a wire. */
export type PortType = "text" | "picture" | "clip";
export type NodeType = "input" | "text" | "picture" | "clip" | "output";

export interface FlowNode {
  id: string;
  type: NodeType;
  x: number;
  y: number;
  /** Model nodes: the advertised model id. */
  model?: string;
  /** Input nodes: the text. Model nodes: an optional instruction put in
   * front of whatever arrives on the prompt port. */
  text?: string;
}

export interface FlowEdge {
  from: string;
  /** Output port name on `from`. */
  op: string;
  to: string;
  /** Input port name on `to`. */
  ip: string;
}

export interface Flow {
  nodes: FlowNode[];
  edges: FlowEdge[];
}

/** Port names and what they carry, per node type. */
export const PORTS: Record<NodeType, { ins: [string, PortType][]; outs: [string, PortType][] }> = {
  input: { ins: [], outs: [["text", "text"]] },
  text: { ins: [["prompt", "text"]], outs: [["text", "text"]] },
  picture: { ins: [["prompt", "text"], ["start from", "picture"]], outs: [["picture", "picture"]] },
  clip: { ins: [["prompt", "text"], ["first frame", "picture"]], outs: [["clip", "clip"]] },
  output: { ins: [["text", "text"], ["picture", "picture"], ["clip", "clip"]], outs: [] },
};

export const KIND_OF: Record<"text" | "picture" | "clip", JobKind> = { text: "llm", picture: "image", clip: "video" };
export const TYPE_OF: Record<JobKind, "text" | "picture" | "clip"> = { llm: "text", image: "picture", video: "clip" };

/** What a model node produced, kept by node id while a run is going. */
export interface NodeOutput {
  text?: string;
  /** Picture and clip results stay on disk; the job id is how to fetch them. */
  jobId?: string;
}

export type NodeState = "idle" | "running" | "done" | "failed";

export interface RunHooks {
  onNode: (id: string, patch: { state?: NodeState; progress?: number; error?: string; jobId?: string; output?: NodeOutput }) => void;
  onStatus: (text: string) => void;
}

/** Everyone serving each kind, cheapest first. */
export type Offers = Record<JobKind, ProviderOption[]>;

/** The cheapest offer for a model, if anyone still serves it. */
export function offerFor(offers: Offers, kind: JobKind, model: string): ProviderOption | undefined {
  return offers[kind].find((o) => o.model === model);
}

/** A rough price for one run: each picture and clip at its advertised
 * price, each text step at four thousand tokens of its rate. */
export function estimate(flow: Flow, offers: Offers): { usd: number; models: number; missing: string[] } {
  let usd = 0;
  let models = 0;
  const missing: string[] = [];
  for (const n of flow.nodes) {
    if (n.type === "input" || n.type === "output" || !n.model) continue;
    models += 1;
    const o = offerFor(offers, KIND_OF[n.type], n.model);
    if (!o) {
      missing.push(n.model);
      continue;
    }
    if (o.unpriced || o.price <= 0) continue;
    usd += n.type === "text" ? (o.price * 4000) / 1_000_000 : o.price;
  }
  return { usd, models, missing };
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export class Stopped extends Error {
  constructor() {
    super("stopped");
  }
}

/** Controls a run in progress. */
export class RunHandle {
  stopped = false;
  jobs = new Set<string>();
  stop() {
    this.stopped = true;
    for (const id of this.jobs) void api.stopJob(id).catch(() => undefined);
  }
}

/**
 * Walk the graph. Nodes whose inputs are all done start together; a node
 * that fails takes its downstream with it and leaves its siblings alone.
 */
export async function runFlow(flow: Flow, offers: Offers, hooks: RunHooks, handle: RunHandle): Promise<void> {
  const done = new Map<string, NodeOutput>();
  const failed = new Set<string>();
  const running = new Set<string>();
  const incoming = (id: string) => flow.edges.filter((e) => e.to === id);
  const name = (n: FlowNode) => n.model ?? (n.type === "input" ? "Input" : "Output");

  for (const n of flow.nodes) hooks.onNode(n.id, { state: "idle", progress: 0, error: undefined, output: undefined });

  while (done.size + failed.size < flow.nodes.length) {
    if (handle.stopped) throw new Stopped();
    const ready = flow.nodes.filter((n) => {
      if (done.has(n.id) || failed.has(n.id) || running.has(n.id)) return false;
      const ins = incoming(n.id);
      if (ins.some((e) => failed.has(e.from))) {
        failed.add(n.id);
        hooks.onNode(n.id, { state: "failed", error: "an earlier step failed" });
        return false;
      }
      return ins.every((e) => done.has(e.from));
    });
    if (ready.length === 0) {
      if (running.size === 0) break; // a cycle, or nothing wired
      await sleep(150);
      continue;
    }
    hooks.onStatus(`Running ${ready.map(name).join(", ")}…`);
    await Promise.all(
      ready.map(async (n) => {
        running.add(n.id);
        hooks.onNode(n.id, { state: "running", progress: 0 });
        try {
          const out = await runNode(n, flow, offers, done, hooks, handle);
          done.set(n.id, out);
          hooks.onNode(n.id, { state: "done", progress: 1, output: out });
        } catch (e) {
          if (e instanceof Stopped) throw e;
          failed.add(n.id);
          hooks.onNode(n.id, { state: "failed", error: e instanceof Error ? e.message : String(e) });
        } finally {
          running.delete(n.id);
        }
      }),
    );
  }
}

async function runNode(
  n: FlowNode,
  flow: Flow,
  offers: Offers,
  done: Map<string, NodeOutput>,
  hooks: RunHooks,
  handle: RunHandle,
): Promise<NodeOutput> {
  const feed = (port: string): NodeOutput | undefined => {
    const e = flow.edges.find((x) => x.to === n.id && x.ip === port);
    return e ? done.get(e.from) : undefined;
  };
  if (n.type === "input") return { text: (n.text ?? "").trim() };
  if (n.type === "output") return {};

  const kind = KIND_OF[n.type];
  if (!n.model) throw new Error("no model chosen");
  const rows = offers[kind];
  const cheapest = rows.find((o) => o.model === n.model);
  if (!cheapest) throw new Error(`nobody is serving ${n.model} right now`);
  const target = targetFor(cheapest, rows);

  const check = await api.potCheck(cheapest.price, cheapest.unpriced, kind);
  if (!check.ready) throw new Error(check.reason || "your wallet cannot cover this step");

  const promptIn = feed("prompt")?.text ?? "";
  const prompt = [n.text?.trim(), promptIn].filter(Boolean).join("\n\n");
  if (!prompt) throw new Error("nothing arrived on the prompt port");

  let payload: JobPayload;
  if (n.type === "text") {
    payload = {
      kind: "llm",
      model_id: n.model,
      messages: [{ role: "user", content: prompt }],
      max_tokens: 8192,
      temperature: 0.7,
    };
  } else {
    const fromPort = n.type === "picture" ? "start from" : "first frame";
    const from = feed(fromPort)?.jobId;
    const bytes = from ? await api.readResultBytes(from).catch(() => undefined) : undefined;
    payload =
      n.type === "picture"
        ? { kind: "image", checkpoint_id: n.model, prompt, ...(bytes ? { from_image: bytes } : {}) }
        : { kind: "video", checkpoint_id: n.model, prompt, ...(bytes ? { from_image: bytes } : {}) };
  }

  const record = await api.submitJob(target.peer_id, payload);
  handle.jobs.add(record.job_id);
  hooks.onNode(n.id, { jobId: record.job_id });
  try {
    // The job pipeline reports through events the screens already listen
    // to; here a short poll is simpler than wiring a listener per node, and
    // a flow is never so wide that it matters.
    for (;;) {
      if (handle.stopped) throw new Stopped();
      const job = await api.getJob(record.job_id);
      if (!job) throw new Error("the job vanished");
      hooks.onNode(n.id, { progress: job.progress });
      if (job.status === "done") break;
      if (job.status === "failed") throw new Error(job.error ?? "the provider failed");
      await sleep(700);
    }
  } finally {
    handle.jobs.delete(record.job_id);
  }
  if (kind === "llm") {
    const result = await api.getResult(record.job_id);
    const text = result?.text?.trim() ?? "";
    if (!text) throw new Error("the model returned no text");
    return { text, jobId: record.job_id };
  }
  return { jobId: record.job_id };
}

const STORE_KEY = "rootmode.flow.current";

export function loadFlow(): Flow | null {
  try {
    const raw = localStorage.getItem(STORE_KEY);
    if (!raw) return null;
    const flow = JSON.parse(raw) as Flow;
    if (!Array.isArray(flow.nodes) || !Array.isArray(flow.edges)) return null;
    return flow;
  } catch {
    return null;
  }
}

export function saveFlow(flow: Flow) {
  try {
    localStorage.setItem(STORE_KEY, JSON.stringify(flow));
  } catch {
    // A browser that refuses storage still gets a working canvas.
  }
}

/** Input → a text model → a picture model → a video model → Output, from
 * whatever is on offer; the shape people reach for first. */
export function starterFlow(offers: Offers): Flow {
  const nodes: FlowNode[] = [{ id: "n1", type: "input", x: 0, y: 260, text: "" }];
  const edges: FlowEdge[] = [];
  let prev = { id: "n1", port: "text" };
  let x = 330;
  const llm = offers.llm[0]?.model;
  const image = offers.image[0]?.model;
  const video = offers.video[0]?.model;
  let id = 2;
  if (llm) {
    nodes.push({ id: `n${id}`, type: "text", x, y: 40, model: llm, text: "Describe one five-second shot for this, in two sentences." });
    edges.push({ from: prev.id, op: prev.port, to: `n${id}`, ip: "prompt" });
    prev = { id: `n${id}`, port: "text" };
    id += 1;
    x += 230;
  }
  let pictureId: string | null = null;
  if (image) {
    nodes.push({ id: `n${id}`, type: "picture", x, y: 170, model: image });
    edges.push({ from: prev.id, op: prev.port, to: `n${id}`, ip: "prompt" });
    pictureId = `n${id}`;
    id += 1;
    x += 230;
  }
  if (video) {
    nodes.push({ id: `n${id}`, type: "clip", x, y: 330, model: video });
    edges.push({ from: prev.id, op: prev.port, to: `n${id}`, ip: "prompt" });
    if (pictureId) edges.push({ from: pictureId, op: "picture", to: `n${id}`, ip: "first frame" });
    id += 1;
    x += 230;
  }
  const out = `n${id}`;
  nodes.push({ id: out, type: "output", x, y: 100 });
  for (const n of nodes) {
    if (n.type === "text") edges.push({ from: n.id, op: "text", to: out, ip: "text" });
    if (n.type === "picture") edges.push({ from: n.id, op: "picture", to: out, ip: "picture" });
    if (n.type === "clip") edges.push({ from: n.id, op: "clip", to: out, ip: "clip" });
  }
  return { nodes, edges };
}
