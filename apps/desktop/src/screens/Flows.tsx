import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, errorText, events } from "../lib/api";
import { useEvent } from "../lib/useEvent";
import {
  estimate,
  KIND_OF,
  loadFlow,
  PORTS,
  RunHandle,
  runFlow,
  saveFlow,
  starterFlow,
  Stopped,
  offerFor,
  nextRun,
  due,
  type Schedule,
  type Flow,
  type FlowNode,
  type NodeOutput,
  type NodeState,
  type NodeType,
  type Offers,
  type PortType,
} from "../lib/flows";
import { describe, priceLabel } from "../lib/models";
import { ClipOptions } from "../components/ClipOptions";
import { clipLabel, type ClipChoice } from "../lib/video";
import type { JobKind } from "../lib/types";

/**
 * Flows: models wired together on a canvas.
 *
 * The palette on the left is the network as it is right now — every model
 * anyone is serving, by kind. Drag one in, wire its ports, press Run: each
 * node becomes an ordinary job, and the results land in the Output node.
 * Ports are typed — text, picture, clip — so a wire can only go where what
 * it carries can be used.
 */

interface Runtime {
  state: NodeState;
  progress: number;
  error?: string;
  jobId?: string;
  output?: NodeOutput;
}

interface Point {
  x: number;
  y: number;
}

interface Temp {
  node: string;
  port: string;
  dir: "in" | "out";
  t: PortType;
  a: Point;
  b: Point;
}

const KINDS: JobKind[] = ["llm", "image", "video"];
const TITLE: Record<NodeType, string> = { input: "Input", image: "Image", output: "Output", text: "Text", picture: "Picture", clip: "Video" };

export function Flows() {
  const [flow, setFlowState] = useState<Flow>(() => loadFlow() ?? { nodes: [], edges: [] });
  const [seeded, setSeeded] = useState(() => loadFlow() !== null);
  const [offers, setOffers] = useState<Offers>({ llm: [], image: [], video: [] });
  const [rt, setRt] = useState<Record<string, Runtime>>({});
  const [sel, setSel] = useState<string | null>(null);
  const [view, setView] = useState({ x: 0, y: 0, k: 1 });
  const [status, setStatus] = useState("Ready");
  const [running, setRunning] = useState(false);
  const [temp, setTemp] = useState<Temp | null>(null);
  const [paths, setPaths] = useState<{ d: string; t: PortType; i: number; live: boolean }[]>([]);
  const [showSchedule, setShowSchedule] = useState(false);
  const handleRef = useRef<RunHandle | null>(null);
  const canvasRef = useRef<HTMLDivElement | null>(null);
  const worldRef = useRef<HTMLDivElement | null>(null);
  const seq = useRef(1);
  const drag = useRef<{ id: string; dx: number; dy: number } | null>(null);
  const pan = useRef<{ x: number; y: number } | null>(null);
  const viewRef = useRef(view);
  viewRef.current = view;
  const flowRef = useRef(flow);
  flowRef.current = flow;

  const setFlow = (f: Flow | ((prev: Flow) => Flow)) => setFlowState(f);

  // The palette is the live network.
  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      const next: Offers = { llm: [], image: [], video: [] };
      await Promise.all(
        KINDS.map((k) =>
          api
            .availableProviders(k)
            .then((rows) => {
              next[k] = rows;
            })
            .catch(() => undefined),
        ),
      );
      if (!cancelled) setOffers(next);
    };
    void load();
    const t = setInterval(load, 8000);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, []);

  // First time here, with something on offer: a starter flow to look at.
  useEffect(() => {
    if (seeded) return;
    if (offers.llm.length + offers.image.length + offers.video.length === 0) return;
    setFlow(starterFlow(offers));
    setSeeded(true);
    setTimeout(fit, 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [offers, seeded]);

  useEffect(() => {
    saveFlow(flow);
  }, [flow]);

  // Fresh ids for new nodes, above anything loaded.
  useEffect(() => {
    const top = flow.nodes.reduce((m, n) => Math.max(m, Number(n.id.replace(/\D/g, "")) || 0), 0);
    if (top >= seq.current) seq.current = top + 1;
  }, [flow.nodes]);

  // ----- geometry: wires are measured from the ports themselves
  function portPos(node: string, port: string, dir: "in" | "out"): Point {
    const world = worldRef.current;
    const el = world?.querySelector<HTMLElement>(`.fl-port[data-node="${node}"][data-port="${port}"][data-dir="${dir}"]`);
    if (!world || !el) {
      const n = flow.nodes.find((x) => x.id === node);
      return { x: n?.x ?? 0, y: n?.y ?? 0 };
    }
    const w = world.getBoundingClientRect();
    const r = el.getBoundingClientRect();
    const k = viewRef.current.k;
    return { x: (r.left - w.left + r.width / 2) / k, y: (r.top - w.top + r.height / 2) / k };
  }
  const curve = (a: Point, b: Point) => {
    const dx = Math.max(40, Math.abs(b.x - a.x) / 2);
    return `M${a.x},${a.y} C${a.x + dx},${a.y} ${b.x - dx},${b.y} ${b.x},${b.y}`;
  };
  useLayoutEffect(() => {
    const next = flow.edges.map((e, i) => {
      const from = flow.nodes.find((n) => n.id === e.from);
      const t = from ? (PORTS[from.type].outs.find((p) => p[0] === e.op)?.[1] ?? "text") : "text";
      const live = rt[e.from]?.state === "done" && rt[e.to]?.state === "running";
      return { d: curve(portPos(e.from, e.op, "out"), portPos(e.to, e.ip, "in")), t, i, live };
    });
    setPaths(next);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [flow, rt, view, offers]);

  function toWorld(e: { clientX: number; clientY: number }): Point {
    const r = canvasRef.current!.getBoundingClientRect();
    const v = viewRef.current;
    return { x: (e.clientX - r.left - v.x) / v.k, y: (e.clientY - r.top - v.y) / v.k };
  }

  function fit() {
    const world = worldRef.current;
    const cv = canvasRef.current;
    if (!world || !cv) return;
    const els = [...world.querySelectorAll<HTMLElement>(".fl-node")];
    if (!els.length) return;
    let x0 = Infinity,
      y0 = Infinity,
      x1 = -Infinity,
      y1 = -Infinity;
    for (const el of els) {
      const n = flowRef.current.nodes.find((m) => m.id === el.dataset.id);
      if (!n) continue;
      x0 = Math.min(x0, n.x);
      y0 = Math.min(y0, n.y);
      x1 = Math.max(x1, n.x + el.offsetWidth);
      y1 = Math.max(y1, n.y + el.offsetHeight);
    }
    const r = cv.getBoundingClientRect();
    const pad = 28;
    const k = Math.min(1, (r.width - pad * 2) / (x1 - x0), (r.height - pad * 2) / (y1 - y0));
    setView({
      k,
      x: Math.round((r.width - (x1 - x0) * k) / 2 - x0 * k),
      y: Math.round((r.height - (y1 - y0) * k) / 2 - y0 * k),
    });
  }

  // Wheel zoom around the cursor. React's own onWheel is passive, so it
  // cannot stop the page scrolling; a native listener can.
  useEffect(() => {
    const cv = canvasRef.current;
    if (!cv) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = cv.getBoundingClientRect();
      const mx = e.clientX - r.left,
        my = e.clientY - r.top;
      setView((v) => {
        const k = Math.min(2, Math.max(0.35, v.k * (e.deltaY < 0 ? 1.1 : 0.9)));
        return { k, x: mx - (mx - v.x) * (k / v.k), y: my - (my - v.y) * (k / v.k) };
      });
    };
    cv.addEventListener("wheel", onWheel, { passive: false });
    return () => cv.removeEventListener("wheel", onWheel);
  }, []);

  // ----- mouse: ports wire, headers drag, empty canvas pans
  function onMouseDown(e: React.MouseEvent) {
    const target = e.target as HTMLElement;
    const port = target.closest<HTMLElement>(".fl-port");
    const head = target.closest<HTMLElement>(".fl-head");
    const node = target.closest<HTMLElement>(".fl-node");
    if (target.closest("textarea, button, input, select, label")) return;
    const pt = toWorld(e);
    if (port && !running) {
      const t = port.dataset.t as PortType;
      const dir = port.dataset.dir as "in" | "out";
      setTemp({ node: port.dataset.node!, port: port.dataset.port!, dir, t, a: portPos(port.dataset.node!, port.dataset.port!, dir), b: pt });
      e.preventDefault();
      return;
    }
    if (head && node) {
      const n = flow.nodes.find((x) => x.id === node.dataset.id);
      if (!n) return;
      setSel(n.id);
      drag.current = { id: n.id, dx: pt.x - n.x, dy: pt.y - n.y };
      e.preventDefault();
      return;
    }
    if (node) {
      setSel(node.dataset.id ?? null);
      return;
    }
    pan.current = { x: e.clientX - view.x, y: e.clientY - view.y };
    setSel(null);
  }
  useEffect(() => {
    const move = (e: MouseEvent) => {
      if (drag.current) {
        const pt = toWorld(e);
        const d = drag.current;
        setFlow((f) => ({
          ...f,
          nodes: f.nodes.map((n) =>
            n.id === d.id ? { ...n, x: Math.round((pt.x - d.dx) / 10) * 10, y: Math.round((pt.y - d.dy) / 10) * 10 } : n,
          ),
        }));
      } else if (pan.current) {
        const p = pan.current;
        setView((v) => ({ ...v, x: e.clientX - p.x, y: e.clientY - p.y }));
      } else {
        setTemp((t) => (t ? { ...t, b: toWorld(e) } : t));
      }
    };
    const up = (e: MouseEvent) => {
      const target = (e.target as HTMLElement | null)?.closest?.(".fl-port") as HTMLElement | null;
      setTemp((t) => {
        if (t && target && target.dataset.t === t.t && target.dataset.dir !== t.dir && target.dataset.node !== t.node) {
          const out = t.dir === "out" ? { n: t.node, p: t.port } : { n: target.dataset.node!, p: target.dataset.port! };
          const inn = t.dir === "in" ? { n: t.node, p: t.port } : { n: target.dataset.node!, p: target.dataset.port! };
          setFlow((f) => ({
            ...f,
            // One wire per input port.
            edges: [...f.edges.filter((x) => !(x.to === inn.n && x.ip === inn.p)), { from: out.n, op: out.p, to: inn.n, ip: inn.p }],
          }));
        }
        return null;
      });
      drag.current = null;
      pan.current = null;
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Delete removes the selected node, unless you are typing.
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (running || !sel) return;
      if ((e.target as HTMLElement | null)?.closest?.("textarea, input")) return;
      if (e.key === "Delete" || e.key === "Backspace") removeNode(sel);
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sel, running]);

  function removeNode(id: string) {
    setFlow((f) => ({ nodes: f.nodes.filter((n) => n.id !== id), edges: f.edges.filter((e) => e.from !== id && e.to !== id) }));
    setSel((s) => (s === id ? null : s));
  }
  function addNode(type: NodeType, model?: string) {
    const cv = canvasRef.current;
    const r = cv?.getBoundingClientRect() ?? { width: 800, height: 500 };
    const v = viewRef.current;
    const x = Math.round(((r.width / 2 - v.x) / v.k - 100 + (Math.random() * 60 - 30)) / 10) * 10;
    const y = Math.round(((r.height / 2 - v.y) / v.k - 40 + (Math.random() * 60 - 30)) / 10) * 10;
    const id = `n${seq.current++}`;
    setFlow((f) => ({ ...f, nodes: [...f.nodes, { id, type, x, y, model, text: "" }] }));
    setSel(id);
  }
  function setText(id: string, text: string) {
    setFlow((f) => ({ ...f, nodes: f.nodes.map((n) => (n.id === id ? { ...n, text } : n)) }));
  }
  function setVideo(id: string, video: ClipChoice) {
    setFlow((f) => ({ ...f, nodes: f.nodes.map((n) => (n.id === id ? { ...n, video } : n)) }));
  }

  // Pictures dropped on the window land on the canvas as Image nodes, under
  // the cursor. The OS hands the files to the backend, which keeps them and
  // tells us where the drop was; documents are the chat's business.
  useEvent(events.onFilesDropped, (outcome) => {
    if (outcome.pictures.length > 0) {
      const cv = canvasRef.current;
      const r = cv?.getBoundingClientRect();
      const v = viewRef.current;
      const inside =
        outcome.at && r && outcome.at.x >= r.left && outcome.at.x <= r.right && outcome.at.y >= r.top && outcome.at.y <= r.bottom;
      const base = inside
        ? toWorld({ clientX: outcome.at!.x, clientY: outcome.at!.y })
        : { x: ((r?.width ?? 800) / 2 - v.x) / v.k, y: ((r?.height ?? 500) / 2 - v.y) / v.k };
      const added = outcome.pictures.map((p, i) => ({
        id: `n${seq.current++}`,
        type: "image" as const,
        x: Math.round((base.x - 90 + i * 24) / 10) * 10,
        y: Math.round((base.y - 20 + i * 24) / 10) * 10,
        picture: { id: p.id, name: p.name, mime: p.mime },
      }));
      setFlow((f) => ({ ...f, nodes: [...f.nodes, ...added] }));
      setSel(added[added.length - 1].id);
      setStatus(added.length === 1 ? `Added ${added[0].picture.name}` : `Added ${added.length} pictures`);
    }
    const notes = [...outcome.rejected];
    if (outcome.attached.length > 0) notes.push("Documents go to a chat; drop them on the Text screen.");
    if (notes.length > 0) setStatus(notes.join(" · "));
  });

  // ----- run
  async function run() {
    if (running) return;
    const handle = new RunHandle();
    handleRef.current = handle;
    setRunning(true);
    setRt({});
    try {
      await runFlow(
        flowRef.current,
        offers,
        {
          onNode: (id, patch) =>
            setRt((prev) => ({ ...prev, [id]: { ...(prev[id] ?? { state: "idle", progress: 0 }), ...patch } })),
          onStatus: setStatus,
        },
        handle,
      );
      setStatus("Done");
    } catch (e) {
      setStatus(e instanceof Stopped ? "Stopped" : `Something went wrong: ${errorText(e)}`);
    } finally {
      setRunning(false);
      handleRef.current = null;
    }
  }
  function stop() {
    handleRef.current?.stop();
  }

  // The timer. Checked every twenty seconds; a due minute runs the flow
  // once, and a once-only schedule switches itself off afterwards.
  const runRef = useRef<() => Promise<void>>(async () => undefined);
  runRef.current = run;
  useEffect(() => {
    const tick = () => {
      const f = flowRef.current;
      if (!f.schedule || running || !due(f.schedule)) return;
      const now = Date.now();
      setFlow((prev) => ({
        ...prev,
        schedule: prev.schedule
          ? { ...prev.schedule, lastRun: now, enabled: prev.schedule.mode === "daily" }
          : prev.schedule,
      }));
      setStatus("Started by the timer");
      void runRef.current();
    };
    const t = setInterval(tick, 20_000);
    tick();
    return () => clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running]);

  const est = useMemo(() => estimate(flow, offers), [flow, offers]);
  const next = nextRun(flow.schedule);
  const setSchedule = (patch: Partial<Schedule>) =>
    setFlow((f) => ({ ...f, schedule: { enabled: false, mode: "daily", ...(f.schedule ?? {}), ...patch } }));
  const nothingOnOffer = KINDS.every((k) => offers[k].length === 0);

  return (
    <div className="flows">
      <aside className="flows-side">
        <div className="flows-side-head">Add a node</div>
        <div className="flows-palette">
          {KINDS.map((k) => {
            const seen = new Set<string>();
            const rows = offers[k].filter((o) => (seen.has(o.model) ? false : (seen.add(o.model), true)));
            const type = k === "llm" ? "text" : k === "image" ? "picture" : "clip";
            return (
              <div key={k}>
                <div className="flows-pal-k">
                  <i style={{ background: `var(--t-${type})` }} />
                  {k === "llm" ? "Text models" : k === "image" ? "Picture models" : "Video models"}
                </div>
                {rows.length === 0 && <div className="flows-pal-empty">nobody online</div>}
                {rows.map((o) => {
                  const free = o.unpriced || o.price <= 0;
                  return (
                    <button key={o.model} className="flows-pal" onClick={() => addNode(type, o.model)} title={o.model} disabled={running}>
                      <span>{describe(o.model).name}</span>
                      <span className={`s${free ? " free" : ""}`}>{free ? "free" : o.price.toFixed(2)}</span>
                    </button>
                  );
                })}
              </div>
            );
          })}
          <div className="flows-pal-k">
            <i style={{ background: "var(--text-3)" }} />
            Plain
          </div>
          <button className="flows-pal" onClick={() => addNode("input")} disabled={running}>
            <span>Input</span>
            <span className="s">text</span>
          </button>
          <button className="flows-pal" onClick={() => addNode("output")} disabled={running}>
            <span>Output</span>
            <span className="s">any</span>
          </button>
          <div className="flows-pal-hint">
            <b>Image</b> — drop a picture anywhere on the canvas to start a model from it.
          </div>
        </div>
        <div className="flows-side-foot">
          <span><i style={{ background: "var(--t-text)" }} />text</span>
          <span><i style={{ background: "var(--t-picture)" }} />picture</span>
          <span><i style={{ background: "var(--t-clip)" }} />clip</span>
        </div>
      </aside>

      <section className="flows-main">
        <div className="flows-bar">
          <span className="name">Flow</span>
          <span className="cost">
            {est.models === 0 ? (
              "add a model to see the cost"
            ) : (
              <>
                about <b>{est.usd.toFixed(2)} USDC</b> a run · {est.models} model{est.models === 1 ? "" : "s"}
                {est.missing.length > 0 && <span className="bad"> · nobody serves {est.missing.map((m) => describe(m).name).join(", ")}</span>}
              </>
            )}
          </span>
          <span className="sp" />
          <span className="hint">{status}</span>
          <span className="flows-sched">
            <button
              className={`btn sm${flow.schedule?.enabled ? " on" : ""}`}
              onClick={() => setShowSchedule((v) => !v)}
              title="Run this flow by itself at a set time, while the app is open"
            >
              {next ? `Timer · ${fmtNext(next)}` : "Timer"}
            </button>
            {showSchedule && (
              <div className="flows-sched-pop">
                <label className="row">
                  <input
                    type="radio"
                    name="sched-mode"
                    checked={(flow.schedule?.mode ?? "daily") === "daily"}
                    onChange={() => setSchedule({ mode: "daily" })}
                  />
                  <span>Every day at</span>
                  <input
                    type="time"
                    value={flow.schedule?.time ?? "09:00"}
                    onChange={(e) => setSchedule({ mode: "daily", time: e.target.value })}
                  />
                </label>
                <label className="row">
                  <input
                    type="radio"
                    name="sched-mode"
                    checked={flow.schedule?.mode === "once"}
                    onChange={() => setSchedule({ mode: "once" })}
                  />
                  <span>Once at</span>
                  <input
                    type="datetime-local"
                    value={flow.schedule?.at ?? ""}
                    onChange={(e) => setSchedule({ mode: "once", at: e.target.value })}
                  />
                </label>
                <label className="row toggle">
                  <input
                    type="checkbox"
                    checked={!!flow.schedule?.enabled}
                    onChange={(e) => setSchedule({ enabled: e.target.checked })}
                  />
                  <span>{flow.schedule?.enabled ? "On" : "Off"}</span>
                </label>
                <div className="note">
                  {next
                    ? `Next run ${next.toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit", day: "numeric", month: "short" })}.`
                    : "Set a time and switch it on."}{" "}
                  Runs only while rootmode is open; each run locks and pays like a run you start yourself.
                </div>
              </div>
            )}
          </span>
          <button className="btn sm" onClick={fit}>Fit</button>
          <button
            className="btn sm"
            disabled={running}
            onClick={() => {
              setFlow((f) => ({ nodes: f.nodes.filter((n) => n.type === "input" || n.type === "output"), edges: [] }));
              setRt({});
            }}
          >
            Clear
          </button>
          {running ? (
            <button className="btn sm danger" onClick={stop}>Stop</button>
          ) : (
            <button className="btn sm primary" onClick={() => void run()} disabled={est.models === 0 || est.missing.length > 0}>
              Run
            </button>
          )}
        </div>

        <div className={`flows-canvas${pan.current ? " panning" : ""}`} ref={canvasRef} onMouseDown={onMouseDown}>
          <div className="flows-world" ref={worldRef} style={{ transform: `translate(${view.x}px,${view.y}px) scale(${view.k})` }}>
            <svg className="flows-edges">
              {paths.map((p) => (
                <path
                  key={p.i}
                  d={p.d}
                  stroke={`var(--t-${p.t})`}
                  className={p.live ? "live" : ""}
                  onClick={() => {
                    if (running) return;
                    setFlow((f) => ({ ...f, edges: f.edges.filter((_, j) => j !== p.i) }));
                  }}
                >
                  <title>Click to disconnect</title>
                </path>
              ))}
              {temp && <path d={curve(temp.a, temp.b)} stroke={`var(--t-${temp.t})`} className="temp" />}
            </svg>
            <div>
              {flow.nodes.map((n) => (
                <Node
                  key={n.id}
                  n={n}
                  rt={rt[n.id]}
                  offers={offers}
                  selected={sel === n.id}
                  running={running}
                  glow={temp ? { t: temp.t, dir: temp.dir === "out" ? "in" : "out", not: temp.node } : null}
                  onRemove={() => removeNode(n.id)}
                  onText={(t) => setText(n.id, t)}
                  onVideo={(v) => setVideo(n.id, v)}
                  incoming={flow.edges.filter((e) => e.to === n.id).map((e) => ({ port: e.ip, from: rt[e.from]?.output }))}
                />
              ))}
            </div>
          </div>
          {flow.nodes.length === 0 && (
            <div className="flows-empty">
              {nothingOnOffer ? "Nobody is offering anything right now." : "Add nodes from the left and wire them up."}
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

function fmtNext(d: Date): string {
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (sameDay) return time;
  const tomorrow = new Date(now);
  tomorrow.setDate(now.getDate() + 1);
  if (d.toDateString() === tomorrow.toDateString()) return `tomorrow ${time}`;
  return d.toLocaleDateString([], { day: "numeric", month: "short" }) + " " + time;
}

function Node({
  n,
  rt,
  offers,
  selected,
  running,
  glow,
  onRemove,
  onText,
  onVideo,
  incoming,
}: {
  n: FlowNode;
  rt?: Runtime;
  offers: Offers;
  selected: boolean;
  running: boolean;
  glow: { t: PortType; dir: "in" | "out"; not: string } | null;
  onRemove: () => void;
  onText: (t: string) => void;
  onVideo: (v: ClipChoice) => void;
  incoming: { port: string; from?: NodeOutput }[];
}) {
  const P = PORTS[n.type];
  const rows = Math.max(P.ins.length, P.outs.length);
  const state = rt?.state ?? "idle";
  const color =
    n.type === "input" ? "var(--t-text)" : n.type === "output" ? "var(--text-3)" : n.type === "image" ? "var(--t-picture)" : `var(--t-${n.type})`;
  const offer =
    n.model && n.type !== "input" && n.type !== "image" && n.type !== "output" ? offerFor(offers, KIND_OF[n.type], n.model) : undefined;
  const title = n.model ? describe(n.model).name : n.type === "image" ? n.picture?.name ?? "Image" : TITLE[n.type];
  const isModel = n.type === "text" || n.type === "picture" || n.type === "clip";

  return (
    <div
      className={`fl-node ${n.type}${selected ? " sel" : ""} ${state}`}
      style={{ left: n.x, top: n.y }}
      data-id={n.id}
    >
      <div className="fl-head">
        <span className="dot" style={{ background: color }} />
        <span className="t">{title}</span>
        {state === "done" && <span className="ok">✓</span>}
        {!running && (
          <button className="x" title="Remove this node (or select it and press Delete)" aria-label={`Remove ${title}`} onClick={onRemove}>
            ×
          </button>
        )}
      </div>
      <div className="fl-rows">
        {Array.from({ length: rows }, (_, i) => (
          <div className="fl-row" key={i}>
            {P.ins[i] && <span className="l">{P.ins[i][0]}</span>}
            {P.outs[i] && <span className="r">{P.outs[i][0]}</span>}
          </div>
        ))}
        {P.ins.map((p, i) => (
          <div
            key={`in-${p[0]}`}
            className={`fl-port in${glow && glow.dir === "in" && glow.t === p[1] && glow.not !== n.id ? " ok" : ""}`}
            data-node={n.id}
            data-port={p[0]}
            data-dir="in"
            data-t={p[1]}
            style={{ top: 2 + i * 22 + 4.5 }}
          />
        ))}
        {P.outs.map((p, i) => (
          <div
            key={`out-${p[0]}`}
            className={`fl-port out${glow && glow.dir === "out" && glow.t === p[1] && glow.not !== n.id ? " ok" : ""}`}
            data-node={n.id}
            data-port={p[0]}
            data-dir="out"
            data-t={p[1]}
            style={{ top: 2 + i * 22 + 4.5 }}
          />
        ))}
      </div>
      <div className="fl-body">
        {n.type === "input" && (
          <textarea value={n.text ?? ""} placeholder="Say what you want…" onChange={(e) => onText(e.target.value)} disabled={running} />
        )}
        {n.type === "image" && (n.picture ? <Media pictureId={n.picture.id} /> : <div className="bad">no picture</div>)}
        {isModel && (
          <>
            <div className="line">
              {offer ? (
                <>
                  {describe(n.model!).maker ?? ""}
                  <span className="m"> · {priceLabel(offer)}</span>
                </>
              ) : (
                <span className="bad">nobody serves this right now</span>
              )}
            </div>
            {n.type === "clip" && offer?.video && !offer.video.first_frame && incoming.some((i) => i.port === "first frame") && (
              <div className="bad">This model cannot start from a picture — disconnect the first-frame wire.</div>
            )}
            {n.type === "clip" &&
              (offer?.video ? (
                <ClipOptions compact offer={offer.video} choice={n.video ?? {}} onChange={onVideo} currency={offer.currency} disabled={running} />
              ) : (
                <div className="line m">{clipLabel(offer?.video, n.video ?? {})}</div>
              ))}
            {n.type === "text" && (
              <textarea
                className="instr"
                value={n.text ?? ""}
                placeholder="Instruction, in front of what arrives (optional)"
                onChange={(e) => onText(e.target.value)}
                disabled={running}
              />
            )}
          </>
        )}
        {n.type === "output" && (
          <div className="fl-results">
            {incoming.filter((i) => i.from).length === 0 ? (
              <div className="empty">Whatever is wired in lands here.</div>
            ) : (
              incoming.map((i) =>
                i.from?.text && i.port === "text" ? (
                  <div className="res text" key={i.port}>{i.from.text}</div>
                ) : i.from?.jobId && i.port !== "text" ? (
                  <Media key={i.port} jobId={i.from.jobId} />
                ) : i.from?.pictureId && i.port === "picture" ? (
                  <Media key={i.port} pictureId={i.from.pictureId} />
                ) : null,
              )
            )}
          </div>
        )}
        {state === "failed" && rt?.error && <div className="bad">{rt.error}</div>}
        {state === "running" && (
          <div className="prog">
            <i style={{ width: `${Math.max(4, (rt?.progress ?? 0) * 100)}%` }} />
          </div>
        )}
      </div>
    </div>
  );
}

/** A picture or clip a node made, straight off disk — or one that was brought. */
function Media({ jobId, pictureId }: { jobId?: string; pictureId?: string }) {
  const [src, setSrc] = useState<string | null>(null);
  const [gone, setGone] = useState(false);
  useEffect(() => {
    let cancelled = false;
    setSrc(null);
    setGone(false);
    const load = jobId ? api.readResultImage(jobId) : pictureId ? api.readPicture(pictureId) : Promise.reject(new Error("nothing"));
    load.then((s) => !cancelled && setSrc(s)).catch(() => !cancelled && setGone(true));
    return () => {
      cancelled = true;
    };
  }, [jobId, pictureId]);
  if (gone) return <div className="res empty">This file is no longer on disk.</div>;
  if (!src) return <div className="res empty">Loading…</div>;
  const video = src.startsWith("data:video/");
  return (
    <div className="res media">
      {video ? <video src={src} controls loop muted playsInline /> : <img src={src} alt="" />}
      {jobId && (
        <button className="btn sm ghost" onClick={() => void api.revealResult(jobId).catch(() => undefined)}>
          Show in Finder
        </button>
      )}
    </div>
  );
}
