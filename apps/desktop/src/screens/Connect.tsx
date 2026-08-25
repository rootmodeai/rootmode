import { useEffect, useMemo, useState } from "react";
import { api, errorText } from "../lib/api";
import { useStore } from "../lib/store";
import type { GatewayStatus, ModelOption, ToolStatus } from "../lib/types";

/**
 * Using the network from the editor you already have open.
 *
 * The technical fact is that rootmode can speak two well-known HTTP APIs on
 * loopback. Nobody needs to be told that: what they need is the two lines to
 * paste, for the tool they actually use. So this screen is a switch and a set
 * of recipes, and the addresses are underneath for anyone who wants them.
 */
export function Connect() {
  const { settings, setSetting } = useStore();
  const [status, setStatus] = useState<GatewayStatus | null>(null);
  const [tool, setTool] = useState<ToolKey>("cursor");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [revealed, setRevealed] = useState(false);
  const [models, setModels] = useState<ModelOption[]>([]);
  const [tools, setTools] = useState<ToolStatus[]>([]);
  const [toolBusy, setToolBusy] = useState<string | null>(null);

  const refreshTools = async () => {
    try {
      setTools(await api.listConnectedTools());
    } catch (e) {
      setError(errorText(e));
    }
  };

  useEffect(() => {
    void refreshTools();
  }, []);

  // One flip does the whole job: patch the tool's config, then open it
  // already running against the network — nothing to copy, nothing to
  // relaunch by hand.
  async function enableTool(key: string) {
    setToolBusy(key);
    setError(null);
    try {
      await api.connectTool(key);
      await refreshTools();
      await api.launchTool(key);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setToolBusy(null);
    }
  }

  async function disableTool(key: string) {
    setToolBusy(key);
    setError(null);
    try {
      await api.disconnectTool(key);
      await refreshTools();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setToolBusy(null);
    }
  }

  const refresh = async () => {
    try {
      setStatus(await api.gatewayStatus());
    } catch (e) {
      setError(errorText(e));
    }
  };

  useEffect(() => {
    const load = () =>
      void api.availableModels("llm").then(setModels).catch(() => undefined);
    load();
    // Providers come and go; a stale list here means a snippet naming a model
    // nobody serves any more.
    const t = setInterval(load, 8000);
    return () => clearInterval(t);
  }, []);

  useEffect(() => {
    void refresh();
    // The count of served requests is the only thing here that moves on its
    // own, and it is what tells someone their editor is really connected.
    const t = setInterval(() => void refresh(), 4000);
    return () => clearInterval(t);
  }, []);

  const on = status?.enabled ?? false;

  async function toggle() {
    setBusy(true);
    setError(null);
    try {
      await setSetting("gateway", on ? "false" : "true");
      await refresh();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  async function setPort(port: string) {
    setBusy(true);
    setError(null);
    try {
      await setSetting("gateway_port", port);
      await refresh();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  // The recipes need a model name that actually exists on the network — a
  // placeholder is the single most common thing to get stuck on. Whatever is
  // chosen here is also what an unrecognised name falls through to, so the
  // snippet and the routing can never disagree.
  const model = status?.model ?? models[0]?.model ?? "MODEL-NAME";
  const recipe = useMemo(
    () => (status ? RECIPES[tool](status, model) : null),
    [tool, status, model],
  );

  return (
    <div className="page">
      <div className="page-head">
        <h1>Use it elsewhere</h1>
        <p>
          Let the apps on this computer — Claude Code, Cursor, VS Code and the
          rest — run their work on the network instead of a paid API.
        </p>
      </div>

      {error && <div className="note bad" style={{ marginBottom: 14 }}>{error}</div>}

      <div className="card">
        <div className="row">
          <span className={`dot ${status?.running ? "ok" : "off"}`} />
          <strong>{on ? "Other apps can connect" : "Off"}</strong>
          <div className="spacer" />
          <button className="btn primary" disabled={busy} onClick={() => void toggle()}>
            {busy ? "…" : on ? "Turn off" : "Turn on"}
          </button>
        </div>

        <p style={{ color: "var(--text-3)", fontSize: 13, margin: "10px 0 0" }}>
          Only programs on this computer can reach it, and only with the key
          below. Nothing is exposed to the internet.
        </p>

        {status?.error && <div className="note bad" style={{ marginTop: 10 }}>{status.error}</div>}

        {on && status?.running && (
          <div className="row" style={{ marginTop: 12, color: "var(--text-3)", fontSize: 13 }}>
            <span className="tag ok">listening</span>
            <span className="mono">{status.base_url}</span>
            <div className="spacer" />
            <span>
              {status.requests === 0
                ? "no requests yet"
                : `${status.requests} request${status.requests === 1 ? "" : "s"} served`}
            </span>
          </div>
        )}
      </div>

      {on && status && (
        <>
          <div className="card" style={{ marginTop: 14 }}>
            <label className="field" style={{ marginBottom: 0 }}>
              <span>Model for other apps</span>
              <select
                value={status.model ?? ""}
                onChange={async (e) => {
                  await setSetting("gateway_model", e.target.value);
                  await refresh();
                }}
              >
                <option value="">
                  {models.length > 0
                    ? `Cheapest available (${models[0].model})`
                    : "Cheapest available"}
                </option>
                {models.map((m) => (
                  <option key={m.model} value={m.model}>
                    {m.model} — {m.peer_label}
                    {m.unpriced
                      ? " · free"
                      : ` · ${m.price.toFixed(2)} ${m.currency} / M tokens`}
                    {m.providers > 1 ? ` · ${m.providers} providers` : ""}
                  </option>
                ))}
              </select>
              <p style={{ color: "var(--text-3)", fontSize: 12.5, margin: "6px 0 0" }}>
                {models.length === 0
                  ? "Nothing is being served right now — turn on a provider and this fills in."
                  : "Used in the setup below, and used when an app asks for a model this network does not have."}
              </p>
            </label>
          </div>

          <div className="card" style={{ marginTop: 14 }}>
            <h2>Enable in one click</h2>
            <p style={{ color: "var(--text-3)", fontSize: 13, margin: "-4px 0 12px" }}>
              Flip one on and it opens already pointed at the network — model
              picked, key filled in, nothing to paste.
            </p>
            <div className="grid" style={{ gap: 2 }}>
              {tools.map((t) => (
                <div key={t.key} className="row" style={{ padding: "9px 0", borderBottom: "1px solid var(--border)" }}>
                  <div style={{ fontWeight: 550, fontSize: 13.5 }}>{t.display_name}</div>
                  {!t.installed && (
                    <span className="tag" style={{ fontSize: 11 }}>Not installed</span>
                  )}
                  <div className="spacer" />
                  <label className="switch" aria-label={`Enable ${t.display_name}`}>
                    <input
                      type="checkbox"
                      checked={t.connected}
                      disabled={!t.installed || toolBusy === t.key}
                      onChange={() => void (t.connected ? disableTool(t.key) : enableTool(t.key))}
                    />
                    <span className="switch-track" />
                  </label>
                </div>
              ))}
            </div>
          </div>

          <div className="card" style={{ marginTop: 14 }}>
            <div className="row" style={{ marginBottom: 12, flexWrap: "wrap", gap: 6 }}>
              {TOOLS.map((t) => (
                <button
                  key={t.key}
                  className={`btn sm${tool === t.key ? " primary" : " ghost"}`}
                  onClick={() => setTool(t.key)}
                >
                  {t.label}
                </button>
              ))}
            </div>

            {recipe && (
              <>
                <p style={{ margin: "0 0 10px", fontSize: 13.5 }}>{recipe.blurb}</p>
                <Snippet text={recipe.snippet} />
                {recipe.note && (
                  <p style={{ color: "var(--text-3)", fontSize: 12.5, margin: "10px 0 0" }}>
                    {recipe.note}
                  </p>
                )}
              </>
            )}
          </div>

          <div className="card" style={{ marginTop: 14 }}>
            <div className="row">
              <strong style={{ fontSize: 14 }}>Your key</strong>
              <div className="spacer" />
              <button className="btn sm ghost" onClick={() => setRevealed((v) => !v)}>
                {revealed ? "Hide" : "Show"}
              </button>
              <button
                className="btn sm"
                onClick={() => void navigator.clipboard.writeText(status.token)}
              >
                Copy
              </button>
              <button
                className="btn sm danger"
                onClick={async () => {
                  setStatus(await api.rotateGatewayToken());
                  setRevealed(true);
                }}
              >
                Replace
              </button>
            </div>
            <div className="mono" style={{ fontSize: 12.5, marginTop: 8, color: "var(--text-2)" }}>
              {revealed ? status.token : "•".repeat(28)}
            </div>
            <p style={{ color: "var(--text-3)", fontSize: 12.5, margin: "8px 0 0" }}>
              Replacing it stops anything still using the old one.
            </p>
          </div>

          <details className="advanced" style={{ marginTop: 14 }}>
            <summary>Details</summary>
            <div className="body" style={{ fontSize: 12.5, lineHeight: 1.9 }}>
              <div className="mono" style={{ color: "var(--text-2)" }}>
                <div>anthropic {status.base_url}/v1/messages</div>
                <div>openai {status.base_url}/v1/chat/completions</div>
                <div>models {status.base_url}/v1/models</div>
              </div>
              <label className="field" style={{ marginTop: 12, maxWidth: 220 }}>
                <span>Port</span>
                <input
                  className="mono"
                  defaultValue={String(settings?.gateway_port ?? status.port)}
                  onBlur={(e) => {
                    const v = e.target.value.trim();
                    if (v && v !== String(status.port)) void setPort(v);
                  }}
                />
              </label>
              <label className="row" style={{ marginTop: 4, gap: 8, cursor: "pointer" }}>
                <input
                  type="checkbox"
                  checked={status.substitute}
                  onChange={async (e) => {
                    await setSetting("gateway_substitute", e.target.checked ? "true" : "false");
                    await refresh();
                  }}
                />
                <span>Answer unknown model names with the model chosen above</span>
              </label>
              <p style={{ color: "var(--text-3)", margin: "8px 0 0" }}>
                Editors ask for model names that no provider here serves —
                Claude Code names a Claude model for its own background work.
                With this on, those go to the model chosen above instead of
                failing, and the reply says which one answered. Turn it off to
                have anything unrecognised refused instead.
              </p>
              <p style={{ color: "var(--text-3)", margin: "8px 0 0" }}>
                Otherwise requests go to whichever provider serves that model
                most cheaply, the same rule the chat window uses. Text only —
                picture generation stays in the Create tab.
              </p>
            </div>
          </details>
        </>
      )}
    </div>
  );
}

function Snippet({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div style={{ position: "relative" }}>
      <pre
        className="mono"
        style={{
          background: "var(--surface-2, #f6f6f4)",
          border: "1px solid var(--line, #e6e6e2)",
          borderRadius: 8,
          padding: "12px 14px",
          margin: 0,
          fontSize: 12.5,
          lineHeight: 1.75,
          overflowX: "auto",
          whiteSpace: "pre",
        }}
      >
        {text}
      </pre>
      <button
        className="btn sm"
        style={{ position: "absolute", top: 8, right: 8 }}
        onClick={() => {
          void navigator.clipboard.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1400);
        }}
      >
        {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}

// ------------------------------------------------------------------ recipes

type ToolKey =
  | "cursor"
  | "vscode"
  | "aider"
  | "other";

const TOOLS: Array<{ key: ToolKey; label: string }> = [
  { key: "cursor", label: "Cursor" },
  { key: "vscode", label: "VS Code" },
  { key: "aider", label: "Aider" },
  { key: "other", label: "Anything else" },
];

interface Recipe {
  blurb: string;
  snippet: string;
  note?: string;
}

/**
 * One recipe per tool with no single config file to patch automatically —
 * Claude Code and Zed moved to the one-click list above once they got a
 * patcher of their own. These are all OpenAI-shaped, so the values are the
 * same three every time; only where they go differs.
 */
const RECIPES: Record<ToolKey, (s: GatewayStatus, model: string) => Recipe> = {
  cursor: (s, model) => ({
    blurb:
      "Cursor → Settings → Models. Turn on “Override OpenAI Base URL”, paste the address and key, then add the model name.",
    snippet: `Base URL   ${s.base_url}/v1
API key    ${s.token}
Model      ${model}`,
    note:
      "Cursor verifies the key by calling the endpoint, so leave rootmode open while you do this.",
  }),

  vscode: (s, model) => ({
    blurb:
      "For the Continue extension, add this to ~/.continue/config.json. Cline and Roo take the same three values in their settings panel.",
    snippet: `{
  "models": [
    {
      "title": "rootmode",
      "provider": "openai",
      "model": "${model}",
      "apiBase": "${s.base_url}/v1",
      "apiKey": "${s.token}"
    }
  ]
}`,
    note: "Continue will not list models on its own; the name above is filled in from what your network is serving.",
  }),

  aider: (s, model) => ({
    blurb: "Run it like this from a terminal.",
    snippet: `export OPENAI_API_BASE=${s.base_url}/v1
export OPENAI_API_KEY=${s.token}
aider --model openai/${model}`,
  }),

  other: (s) => ({
    blurb:
      "Anything that can be pointed at an OpenAI-compatible server will work. Give it these.",
    snippet: `Base URL   ${s.base_url}/v1
API key    ${s.token}

# check it from a terminal:
curl ${s.base_url}/v1/models -H "Authorization: Bearer ${s.token}"`,
    note:
      "If the tool speaks Anthropic's API instead, use the same address without /v1 and send the key as ANTHROPIC_AUTH_TOKEN.",
  }),
};
