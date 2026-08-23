import { useState } from "react";
// import { countryLabel } from "../lib/country";   // see below
import { api, errorText } from "../lib/api";
import { useStore } from "../lib/store";
import type { Peer } from "../lib/types";

/**
 * Who is answering, in plain language.
 *
 * Addresses, keys and hashes are all still here — under "Details", because
 * being able to check is the point, and being made to look is not.
 */
export function Network() {
  const { peers, refreshPeers, settings } = useStore();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [address, setAddress] = useState("");

  const online = peers.filter((p) => p.status === "online");

  async function look() {
    setBusy("all");
    setError(null);
    try {
      await api.discoverPeers();
      await api.probeAllPeers();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
      await refreshPeers();
    }
  }

  async function add() {
    setBusy("add");
    setError(null);
    try {
      const peer = await api.addPeer("", address);
      setAddress("");
      setAdding(false);
      await api.probePeer(peer.id).catch(() => undefined);
      await refreshPeers();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="page">
      <div className="page-head">
        <h1>Providers</h1>
        <p>
          {online.length > 0
            ? `${online.length} online and ready to run your work.`
            : "Nobody is online right now."}
        </p>
      </div>

      {error && <div className="note bad" style={{ marginBottom: 14 }}>{error}</div>}

      <div className="row" style={{ marginBottom: 14 }}>
        <button className="btn" disabled={busy !== null} onClick={() => void look()}>
          {busy === "all" ? "Looking…" : "Look for providers"}
        </button>
        <button className="btn ghost" onClick={() => setAdding((v) => !v)}>
          Add one by address
        </button>
      </div>

      {adding && (
        <div className="card" style={{ marginBottom: 14 }}>
          <label className="field">
            <span>Address</span>
            <input
              className="mono"
              style={{ fontSize: 12.5 }}
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              placeholder="/ip4/192.168.1.50/tcp/4101/p2p/12D3KooW…"
            />
            <p style={{ color: "var(--text-3)", fontSize: 12.5, margin: "6px 0 0" }}>
              The address a worker prints when it starts. You only need this for
              a provider the network cannot find on its own.
            </p>
          </label>
          <div className="row">
            <button className="btn primary" disabled={!address.trim() || busy !== null} onClick={() => void add()}>
              Add
            </button>
            <button className="btn ghost" onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </div>
      )}

      {peers.length === 0 ? (
        <div className="empty">Nothing found yet.</div>
      ) : (
        peers.map((p) => <PeerCard key={p.id} peer={p} isDefault={settings?.default_peer === p.id} />)
      )}
    </div>
  );
}

function PeerCard({ peer, isDefault }: { peer: Peer; isDefault: boolean }) {
  const { refreshPeers, setSetting } = useStore();
  const [busy, setBusy] = useState(false);

  const state =
    peer.status === "online"
      ? { dot: "ok", text: "Online" }
      : peer.status === "mismatch"
        ? { dot: "bad", text: "Not the machine you pinned" }
        : peer.status === "offline"
          ? { dot: "off", text: "Not responding" }
          : { dot: "off", text: "Not checked yet" };

  const can = [
    peer.caps.includes("llm") ? "Text" : null,
    peer.caps.includes("image") ? "Images" : null,
  ].filter(Boolean);

  return (
    <div className="card">
      <div className="row">
        <span className={`dot ${state.dot}`} />
        <strong>{peer.label}</strong>
        {/* Hidden while the network is seeded with our own capacity: a
            location is a claim, and until the nodes are other people's
            machines it is not one worth making. The field is still carried
            and stored — this is the display, not the data.

        {countryLabel(peer.country) && (
          <span className="tag" title={`This worker says it is in ${countryLabel(peer.country)}. Self-declared — rootmode does not look up addresses.`}>
            {countryLabel(peer.country)}
          </span>
        )} */}
        {peer.source === "discovered" && <span className="tag">found on the network</span>}
        {isDefault && <span className="tag ok">default</span>}
        <div className="spacer" />
        <span style={{ color: "var(--text-3)", fontSize: 13 }}>
          {state.text}
          {peer.latency_ms !== null ? ` · ${peer.latency_ms} ms` : ""}
        </span>
      </div>

      <div className="row" style={{ marginTop: 10 }}>
        {can.length > 0 ? (
          can.map((c) => (
            <span key={c} className="tag ok">
              {c}
            </span>
          ))
        ) : (
          <span className="tag">Nothing advertised</span>
        )}
        {peer.models.map((m) => (
          <span key={m.id} className="tag">
            {m.id}
          </span>
        ))}
      </div>

      {peer.last_error && peer.status !== "online" && (
        <div className="note bad" style={{ marginTop: 10 }}>
          {peer.last_error}
        </div>
      )}

      <div className="row" style={{ marginTop: 12 }}>
        <button
          className="btn sm"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              await api.probePeer(peer.id);
            } finally {
              setBusy(false);
              await refreshPeers();
            }
          }}
        >
          {busy ? "Checking…" : "Check now"}
        </button>
        {!isDefault && peer.status === "online" && (
          <button className="btn sm" onClick={() => void setSetting("default_peer", peer.id)}>
            Use by default
          </button>
        )}
        {peer.endpoint !== "mock://local" && (
          <button
            className="btn sm danger"
            onClick={async () => {
              await api.removePeer(peer.id);
              await refreshPeers();
            }}
          >
            Remove
          </button>
        )}
      </div>

      <details className="advanced">
        <summary>Details</summary>
        <div className="body mono" style={{ fontSize: 12, color: "var(--text-2)", lineHeight: 1.9 }}>
          <div>address {peer.endpoint}</div>
          {peer.peer_id && <div>key {peer.peer_id}</div>}
          <div>
            verified{" "}
            {peer.public_key
              ? "yes — pinned to a key you set"
              : "no — pin its key to be sure it is the same machine next time"}
          </div>
          <div>runs up to {peer.max_concurrent} job(s) at once</div>
        </div>
      </details>
    </div>
  );
}
