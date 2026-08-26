import { useEffect, useMemo, useState } from "react";
import { api } from "../lib/api";
import { useChoice, setChoice } from "../lib/choice";
import { describe, priceLabel, searchTerms } from "../lib/models";
import type { JobKind, ProviderOption } from "../lib/types";

/**
 * The models, nested under Chat, Images or Videos in the navigation.
 *
 * Open a section and its models unfold beneath it, by the name people use
 * — Nano Banana, Veo, Claude — with the price per picture, clip or
 * million tokens. Pick one and the screen opens with it set, cursor in
 * the box. The chosen model's providers sit under it for anyone who would
 * rather pin a particular node; a filter appears when the list is long.
 */

interface Row {
  model: string;
  name: string;
  maker: string | null;
  /** Cheapest first — the order the list arrives in. */
  offers: ProviderOption[];
}

function rowsOf(offers: ProviderOption[]): Row[] {
  const out: Row[] = [];
  const seen = new Map<string, Row>();
  for (const o of offers) {
    let r = seen.get(o.model);
    if (!r) {
      const d = describe(o.model);
      r = { model: o.model, name: d.name, maker: d.maker, offers: [] };
      seen.set(o.model, r);
      out.push(r);
    }
    r.offers.push(o);
  }
  return out;
}

export function NavModels({ kind, onPick }: { kind: JobKind; onPick: () => void }) {
  const [offers, setOffers] = useState<ProviderOption[]>([]);
  const [query, setQuery] = useState("");
  const [showProviders, setShowProviders] = useState(false);
  const [chosen] = useChoice(kind);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      void api
        .availableProviders(kind)
        .then((rows) => !cancelled && setOffers(rows))
        .catch(() => undefined);
    load();
    // Providers come and go; a stale list here is a choice that fails on send.
    const t = setInterval(load, 8000);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, [kind]);

  const rows = useMemo(() => rowsOf(offers), [offers]);
  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return rows;
    const words = q.split(/\s+/);
    return rows.filter((r) => {
      const free = r.offers[0].unpriced || r.offers[0].price <= 0 ? "free" : "";
      const haystack = `${searchTerms(r.model)} ${free} ${r.offers.map((o) => o.peer_label).join(" ")}`.toLowerCase();
      return words.every((w) => haystack.includes(w));
    });
  }, [rows, query]);

  const stale =
    chosen !== null &&
    offers.length > 0 &&
    !offers.some((o) => o.peer_id === chosen.peer_id && o.model === chosen.model);

  const pick = (value: ProviderOption | null) => {
    setChoice(kind, value, true);
    setShowProviders(false);
    onPick();
  };

  const noun = kind === "image" ? "pictures" : kind === "video" ? "videos" : "replies";

  return (
    <div className="nav-models">
      {rows.length > 8 && (
        <input
          className="nav-models-search"
          value={query}
          placeholder="Find a model…"
          onChange={(e) => setQuery(e.target.value)}
        />
      )}
      {rows.length === 0 ? (
        <div className="nav-models-empty">Nobody is making {noun} right now.</div>
      ) : (
        <>
          {!query && (
            <button
              className={`nav-model auto${chosen === null ? " active" : ""}`}
              onClick={() => pick(null)}
            >
              <span className="n">Auto</span>
              <span className="s">cheapest</span>
            </button>
          )}
          {matches.length === 0 && (
            <div className="nav-models-empty">Nothing matches “{query}”.</div>
          )}
          {matches.map((r) => {
            const cheapest = r.offers[0];
            const active = chosen?.model === r.model;
            const free = cheapest.unpriced || cheapest.price <= 0;
            return (
              <div key={r.model} className={`nav-model-wrap${active ? " active" : ""}`}>
                <button
                  className={`nav-model${active ? " active" : ""}${active && stale ? " stale" : ""}`}
                  onClick={() => pick({ ...cheapest, pinned: false })}
                  title={[r.maker, r.model].filter(Boolean).join(" · ")}
                >
                  <span className="n">{r.name}</span>
                  <span className={`s${free ? " free" : ""}`}>{free ? "free" : priceLabel(cheapest)}</span>
                </button>
                {active && (
                  <div className="nav-model-more">
                    {stale && <div className="nav-model-stale">That provider has gone offline.</div>}
                    {r.offers.length > 1 ? (
                      <button className="nav-model-link" onClick={() => setShowProviders((v) => !v)}>
                        {chosen?.pinned ? `Pinned to ${chosen.peer_label}` : `${r.offers.length} providers`}
                        {showProviders ? " ▾" : " ▸"}
                      </button>
                    ) : (
                      <div className="nav-model-link quiet">via {cheapest.peer_label}</div>
                    )}
                    {showProviders &&
                      r.offers.map((o) => (
                        <button
                          key={o.peer_id}
                          className={`nav-offer${chosen?.pinned && chosen.peer_id === o.peer_id ? " active" : ""}`}
                          onClick={() => {
                            setChoice(kind, { ...o, pinned: true }, true);
                            onPick();
                          }}
                        >
                          <span className="p">{o.peer_label}</span>
                          <span className="s">
                            {priceLabel(o)}
                            {o.latency_ms !== null ? ` · ${o.latency_ms} ms` : ""}
                          </span>
                        </button>
                      ))}
                    {chosen?.pinned && (
                      <button
                        className="nav-model-link"
                        onClick={() => pick({ ...cheapest, pinned: false })}
                      >
                        Let the app pick the provider
                      </button>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </>
      )}
    </div>
  );
}
