import { useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, errorText } from "../lib/api";
import { usd } from "../components/FundingNotice";
import type { Deposit, ModelUsage, PotStatus } from "../lib/types";

/**
 * The pot, its deposits, and what you have spent in tokens — a money page,
 * not a setting. Caps are still set in MetaMask when you deposit.
 */
export function Wallet() {
  const [error, setError] = useState<string | null>(null);
  const [pot, setPot] = useState<PotStatus | null>(null);
  const [deposits, setDeposits] = useState<Deposit[] | null>(null);
  const [usage, setUsage] = useState<ModelUsage[] | null>(null);

  const loadStatus = () =>
    api
      .potStatus()
      .then(setPot)
      .catch((e) => setError(errorText(e)));

  const loadHistory = () => {
    void api
      .potDeposits()
      .then(setDeposits)
      .catch((e) => setError(errorText(e)));
    void api
      .tokenUsage()
      .then(setUsage)
      .catch((e) => setError(errorText(e)));
  };

  useEffect(() => {
    let cancelled = false;
    const tick = () => {
      api
        .potStatus()
        .then((s) => {
          if (!cancelled) setPot(s);
        })
        .catch(() => undefined);
      api
        .potDeposits()
        .then((d) => {
          if (!cancelled) setDeposits(d);
        })
        .catch(() => undefined);
    };
    tick();
    loadHistory();
    const id = setInterval(tick, 4000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  const totalTokens = (usage ?? []).reduce((n, u) => n + u.tokens, 0);
  const maxTokens = Math.max(1, ...((usage ?? []).map((u) => u.tokens)));

  return (
    <div className="page">
      <div className="page-head">
        <h1>Wallet</h1>
        <p>USDC you deposit is what priced providers bill. Usage is counted on this computer.</p>
      </div>

      {error && (
        <div className="note bad" style={{ marginBottom: 14 }}>
          {error}
        </div>
      )}

      <div className="card">
        <h2>Balance</h2>
        {pot?.reachable && pot.client ? (
          <div style={{ marginBottom: 12 }}>
            <div style={{ fontSize: 28, fontWeight: 750, letterSpacing: "-0.03em" }}>
              {usd(pot.balance_micros)}
            </div>
            <div className="meta" style={{ marginTop: 6 }}>
              max {usd(pot.max_per_job_micros)} / job · {usd(pot.max_per_day_micros)} / day
              {pot.spent_today_micros > 0 ? ` · ${usd(pot.spent_today_micros)} used today` : ""}
              {pot.reserved_micros > 0 ? ` · ${usd(pot.reserved_micros)} locked for workers` : ""}
            </div>
            <p style={{ color: "var(--text-2)", fontSize: 13, margin: "10px 0 0", lineHeight: 1.45 }}>
              A single reply cannot spend more than the per-job amount. Raise
              the limit when you deposit if you want longer answers.
            </p>
          </div>
        ) : pot?.configured && !pot.reachable ? (
          <div className="note" style={{ marginBottom: 12 }}>
            Chain is configured but not reachable. Check the RPC, or start the
            local chain with
            <span className="mono"> ./contracts/local.sh</span>
          </div>
        ) : (
          <div className="note" style={{ marginBottom: 12 }}>
            No balance yet. Deposit USDC in MetaMask to pay priced providers.
          </div>
        )}
        <div className="row">
          <button
            className="btn primary"
            onClick={() =>
              void api.potOpenFund().catch((e) => setError(errorText(e)))
            }
          >
            Deposit in MetaMask
          </button>
          <button
            className="btn"
            onClick={() => {
              setError(null);
              void loadStatus();
              loadHistory();
            }}
          >
            Refresh
          </button>
        </div>
        {pot?.pot && (
          <details className="advanced">
            <summary>Contract addresses</summary>
            <div className="body mono" style={{ fontSize: 12, color: "var(--text-2)", lineHeight: 1.9 }}>
              <div>rpc {pot.rpc}</div>
              <div>pot {pot.pot}</div>
              <div>usdc {pot.usdc}</div>
              <div>app {pot.app_key}</div>
              {pot.client && <div>wallet {pot.client}</div>}
            </div>
          </details>
        )}
      </div>

      <div className="card">
        <h2>Deposits</h2>
        {deposits === null ? (
          <p style={{ color: "var(--text-3)", fontSize: 13.5, margin: 0 }}>Loading…</p>
        ) : deposits.length === 0 ? (
          <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: 0 }}>
            No deposits yet. The first one you make in MetaMask will show up here.
          </p>
        ) : (
          <table className="ledger">
            <thead>
              <tr>
                <th>When</th>
                <th className="num">Amount</th>
                <th>Limits</th>
                <th>Tx</th>
              </tr>
            </thead>
            <tbody>
              {deposits.map((d) => {
                const url = d.url;
                return (
                  <tr key={d.tx_hash}>
                    <td>{formatWhen(d)}</td>
                    <td className="num">{usd(d.amount_micros)}</td>
                    <td className="meta">
                      {usd(d.max_per_job_micros)} / job · {usd(d.max_per_day_micros)} / day
                    </td>
                    <td className="mono">
                      {url ? (
                        <a
                          href={url}
                          onClick={(e) => {
                            e.preventDefault();
                            void openUrl(url);
                          }}
                        >
                          {shortHash(d.tx_hash)}
                        </a>
                      ) : (
                        shortHash(d.tx_hash)
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      <div className="card">
        <h2>Tokens used</h2>
        {usage === null ? (
          <p style={{ color: "var(--text-3)", fontSize: 13.5, margin: 0 }}>Loading…</p>
        ) : usage.length === 0 ? (
          <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: 0 }}>
            No token counts yet. They appear after a provider reports usage on a reply.
          </p>
        ) : (
          <>
            <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: "0 0 12px" }}>
              {totalTokens.toLocaleString()} tokens across {usage.length} model
              {usage.length === 1 ? "" : "s"}
            </p>
            <table className="ledger">
              <thead>
                <tr>
                  <th>Model</th>
                  <th></th>
                  <th className="num">Tokens</th>
                  <th className="num">Replies</th>
                </tr>
              </thead>
              <tbody>
                {usage.map((u) => (
                  <tr key={u.model}>
                    <td>{u.model}</td>
                    <td>
                      <span className="usage-bar" aria-hidden="true">
                        <span style={{ width: `${(u.tokens / maxTokens) * 100}%` }} />
                      </span>
                    </td>
                    <td className="num">{u.tokens.toLocaleString()}</td>
                    <td className="num">{u.replies.toLocaleString()}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </>
        )}
      </div>
    </div>
  );
}

function shortHash(hash: string): string {
  const h = hash.startsWith("0x") ? hash : `0x${hash}`;
  if (h.length < 12) return h;
  return `${h.slice(0, 6)}…${h.slice(-4)}`;
}

function formatWhen(d: Deposit): string {
  if (d.at > 0) {
    return new Date(d.at * 1000).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      year: "numeric",
      hour: "numeric",
      minute: "2-digit",
    });
  }
  return `block ${d.block}`;
}
