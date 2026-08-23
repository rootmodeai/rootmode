import { useState } from "react";
import { api, errorText } from "../lib/api";
import { useStore } from "../lib/store";

/**
 * Everything you might need, ordered by how likely you are to need it.
 * The parts that can lose you something are last, behind a fold, and say so.
 */
export function Settings() {
  const { settings, setSetting, identity, refreshIdentity, refreshPeers } = useStore();
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [secret, setSecret] = useState<string | null>(null);
  const [importValue, setImportValue] = useState("");
  const [downloadDir, setDownloadDir] = useState<string | null>(null);

  if (!settings) return null;
  const dir = downloadDir ?? settings.download_dir;

  async function save(key: string, value: string, said?: string) {
    setError(null);
    setNote(null);
    try {
      await setSetting(key, value);
      setNote(said ?? "Saved");
    } catch (e) {
      setError(errorText(e));
    }
  }

  return (
    <div className="page">
      <div className="page-head">
        <h1>Settings</h1>
      </div>

      {error && <div className="note bad" style={{ marginBottom: 14 }}>{error}</div>}
      {note && <div className="note ok" style={{ marginBottom: 14 }}>{note}</div>}

      <div className="card">
        <h2>Appearance</h2>
        <div className="row">
          {[
            { id: "light", label: "Light" },
            { id: "dark", label: "Dark" },
          ].map((t) => (
            <button
              key={t.id}
              className={`btn sm ${settings.theme === t.id ? "primary" : ""}`}
              onClick={() => void save("theme", t.id, `Switched to ${t.label.toLowerCase()}`)}
            >
              {t.label}
            </button>
          ))}
        </div>
      </div>

      <div className="card">
        <h2>Images</h2>
        <label className="field">
          <span>Save generated images to</span>
          <div className="row">
            <input value={dir} onChange={(e) => setDownloadDir(e.target.value)} style={{ flex: 1 }} />
            <button className="btn" onClick={() => void save("download_dir", dir)}>
              Save
            </button>
          </div>
        </label>
      </div>

      <div className="card">
        <h2>Finding providers</h2>
        <label className="row" style={{ gap: 9 }}>
          <input
            type="checkbox"
            checked={settings.discovery}
            style={{ width: "auto" }}
            onChange={(e) => void save("discovery", String(e.target.checked))}
          />
          <span>Find providers automatically</span>
        </label>
        <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: "8px 0 0" }}>
          {settings.entry_points > 0
            ? "Providers on the wider network and on your own network are found for you."
            : "Only providers on your own network can be found — this build has no network entry point."}
        </p>
        <label className="row" style={{ gap: 9, marginTop: 12 }}>
          <input
            type="checkbox"
            checked={settings.mock_worker}
            style={{ width: "auto" }}
            onChange={(e) => {
              void save("mock_worker", String(e.target.checked), e.target.checked
                ? "Local mock worker is on — pick mock-llm-v0 in chat"
                : "Local mock worker is off");
              void refreshPeers();
            }}
          />
          <span>Show local mock worker (no GPU — for trying payments)</span>
        </label>

        <details className="advanced">
          <summary>Advanced</summary>
          <div className="body">
            <label className="field">
              <span>Entry points (one per line — leave empty for the built-in ones)</span>
              <textarea
                className="mono"
                rows={2}
                style={{ fontSize: 12.5 }}
                defaultValue={settings.bootstrap}
                onBlur={(e) => void save("bootstrap", e.target.value)}
              />
            </label>
            <label className="row" style={{ gap: 9 }}>
              <input
                type="checkbox"
                checked={settings.sign_jobs}
                style={{ width: "auto" }}
                onChange={(e) => void save("sign_jobs", String(e.target.checked))}
              />
              <span>Sign my requests (proves they came from me)</span>
            </label>
          </div>
        </details>
      </div>

      <div className="card">
        <h2>Your account</h2>
        <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: "0 0 10px" }}>
          rootmode has no sign-in. A key on this computer is your identity, and it
          never leaves unless you export it.
        </p>
        <div className="mono" style={{ fontSize: 12, color: "var(--text-3)", wordBreak: "break-all" }}>
          {identity?.peer_id}
        </div>

        <details className="advanced">
          <summary>Back up or move your account</summary>
          <div className="body">
            <div className="note" style={{ marginBottom: 12 }}>
              Anyone who has this key <strong>is</strong> you. Keep it somewhere safe
              and never paste it into a website or a chat.
            </div>

            <div className="row">
              <button
                className="btn"
                onClick={async () => {
                  try {
                    setSecret(secret ? null : await api.exportIdentitySecret());
                  } catch (e) {
                    setError(errorText(e));
                  }
                }}
              >
                {secret ? "Hide" : "Show my key"}
              </button>
              <button
                className="btn danger"
                onClick={async () => {
                  if (!confirmReplace()) return;
                  try {
                    await api.regenerateIdentity();
                    await refreshIdentity();
                    setSecret(null);
                    setNote("New identity created.");
                  } catch (e) {
                    setError(errorText(e));
                  }
                }}
              >
                Start a new identity
              </button>
            </div>

            {secret && (
              <label className="field" style={{ marginTop: 12 }}>
                <span>Your key</span>
                <input
                  className="mono"
                  readOnly
                  value={secret}
                  onFocus={(e) => e.currentTarget.select()}
                />
              </label>
            )}

            <label className="field" style={{ marginTop: 12 }}>
              <span>Restore a key from a backup</span>
              <div className="row">
                <input
                  className="mono"
                  value={importValue}
                  onChange={(e) => setImportValue(e.target.value)}
                  placeholder="64 characters"
                  style={{ flex: 1 }}
                />
                <button
                  className="btn"
                  disabled={importValue.trim().length !== 64}
                  onClick={async () => {
                    if (!confirmReplace()) return;
                    try {
                      await api.importIdentity(importValue.trim());
                      await refreshIdentity();
                      setImportValue("");
                      setNote("Key restored.");
                    } catch (e) {
                      setError(errorText(e));
                    }
                  }}
                >
                  Restore
                </button>
              </div>
            </label>
          </div>
        </details>
      </div>

      <div className="card">
        <details className="advanced">
          <summary>Where things are kept</summary>
          <div className="body mono" style={{ fontSize: 12, color: "var(--text-2)", lineHeight: 1.9 }}>
            <div>{settings.app_data_dir}</div>
            <div>{settings.db_path}</div>
            <div>{settings.key_path}</div>
          </div>
        </details>
      </div>
    </div>
  );
}

function confirmReplace(): boolean {
  return window.confirm(
    "This replaces the key on this computer. If you have not backed it up, that identity is gone for good. Continue?",
  );
}
