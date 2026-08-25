import { useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, errorText } from "../lib/api";
import { usd, usdExact } from "../components/FundingNotice";
import type { Deposit, ModelUsage, PotStatus, SpendEntry } from "../lib/types";

/**
 * The pot, its deposits, and what every job cost — a money page, not a
 * setting. Caps are still set in MetaMask when you deposit.
 */
export function Wallet() {
  const [error, setError] = useState<string | null>(null);
  const [pot, setPot] = useState<PotStatus | null>(null);
  const [deposits, setDeposits] = useState<Deposit[] | null>(null);
  const [usage, setUsage] = useState<ModelUsage[] | null>(null);
  const [spend, setSpend] = useState<SpendEntry[] | null>(null);

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
    void api
      .spendHistory()
      .then(setSpend)
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
      // Pull any new settlement transactions for this wallet from the chain
      // (throttled in the backend), then re-read the ledger so a reply that
      // was just collected shows its transaction.
      api.syncSettlements().catch(() => undefined);
      // Local sqlite reads — cheap, and a reply billed seconds ago should
      // already be on the money page when the user comes to check it.
      api
        .tokenUsage()
        .then((u) => {
          if (!cancelled) setUsage(u);
        })
        .catch(() => undefined);
      api
        .spendHistory()
        .then((s) => {
          if (!cancelled) setSpend(s);
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
  const totalCost = (usage ?? []).reduce((n, u) => n + u.cost_micros, 0);

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
              {usd(pot.balance_micros + pot.reserved_micros)}
            </div>
            <div className="meta" style={{ marginTop: 6 }}>
              {pot.reserved_micros > 0
                ? `${usd(pot.balance_micros)} available · ${usd(pot.reserved_micros)} locked`
                : `${usd(pot.balance_micros)} available`}
              {` · max ${usd(pot.max_per_job_micros)} / job · ${usd(pot.max_per_day_micros)} / day`}
              {pot.spent_today_micros > 0 ? ` · ${usd(pot.spent_today_micros)} used today` : ""}
            </div>
            <p style={{ color: "var(--text-2)", fontSize: 13, margin: "10px 0 0", lineHeight: 1.45 }}>
              A single reply cannot spend more than the per-job amount. Raise
              the limit when you deposit if you want longer answers.
            </p>
          </div>
        ) : pot?.configured && !pot.reachable ? (
          <div className="note" style={{ marginBottom: 12 }}>
            Can't reach Base right now. Check the network and try again.
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
        <h2>Usage &amp; spend</h2>
        {spend === null || usage === null ? (
          <p style={{ color: "var(--text-3)", fontSize: 13.5, margin: 0 }}>Loading…</p>
        ) : spend.length === 0 && usage.length === 0 ? (
          <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: 0 }}>
            Nothing yet. Every reply from a priced provider will be listed here
            with the exact USDC it deducted from your balance; token counts
            appear once a provider reports usage.
          </p>
        ) : (
          <>
            <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: "0 0 12px" }}>
              {totalTokens.toLocaleString()} tokens across {usage.length} model
              {usage.length === 1 ? "" : "s"}
              {totalCost > 0 ? ` · ${usdExact(totalCost)} spent` : " · nothing billed"}
              . Each row is one reply, the exact USDC it deducted, and the
              on-chain transaction that collected it. Charges are collected in
              batches, so several replies can share one transaction; a reply
              still waiting on its batch shows as pending. Free providers deduct
              nothing and are not listed.
            </p>
            {spend.length === 0 ? (
              <p style={{ color: "var(--text-2)", fontSize: 13.5, margin: 0 }}>
                Nothing billed yet — everything so far ran on free providers.
              </p>
            ) : (
              <table className="ledger">
                <thead>
                  <tr>
                    <th>When</th>
                    <th>Model</th>
                    <th>Provider</th>
                    <th className="num">Tokens</th>
                    <th className="num">Cost</th>
                    <th>Collected</th>
                  </tr>
                </thead>
                <tbody>
                  {spend.map((s, i) => (
                    <tr key={s.job_id ?? `row-${i}`}>
                      <td>{formatStamp(s.at)}</td>
                      <td>{s.model}</td>
                      <td className="meta">{s.peer ?? "—"}</td>
                      <td className="num">{s.tokens != null ? s.tokens.toLocaleString() : "—"}</td>
                      <td className="num">{usdExact(s.cost_micros)}</td>
                      <td className="mono">
                        {s.settle_tx && s.settle_url ? (
                          <a
                            href={s.settle_url}
                            title={`Collected in block ${s.settle_block ?? "?"}`}
                            onClick={(e) => {
                              e.preventDefault();
                              void openUrl(s.settle_url as string);
                            }}
                          >
                            {shortHash(s.settle_tx)}
                          </a>
                        ) : s.settle_tx ? (
                          shortHash(s.settle_tx)
                        ) : s.cumulative_micros != null ? (
                          <span className="meta">pending</span>
                        ) : (
                          <span className="meta" title="Recorded before settlement tracking existed">—</span>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
            {usage.length > 0 && (
              <details className="advanced">
                <summary>Totals by model</summary>
                <div className="body">
                  <table className="ledger">
                    <thead>
                      <tr>
                        <th>Model</th>
                        <th></th>
                        <th className="num">Tokens</th>
                        <th className="num">Replies</th>
                        <th className="num">Cost</th>
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
                          <td className="num">{u.cost_micros > 0 ? usdExact(u.cost_micros) : "—"}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </details>
            )}
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

function formatStamp(at: number): string {
  return new Date(at * 1000).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

function formatWhen(d: Deposit): string {
  if (d.at > 0) {
    return formatStamp(d.at);
  }
  return `block ${d.block}`;
}
