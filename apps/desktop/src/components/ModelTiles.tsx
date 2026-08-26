import { useMemo, useState } from "react";
import { describe, priceLabel, searchTerms } from "../lib/models";
import type { JobKind, ProviderOption } from "../lib/types";

/**
 * The models, as tiles beside the conversation.
 *
 * Always on screen, one click each: the name people use for the model,
 * who makes it, what it costs per picture, clip or million tokens. The
 * chosen one is marked; "Auto" hands the choice to the app. A model's
 * providers sit under its tile once it is chosen, for anyone who would
 * rather pin a particular node — the choice most people never make.
 */

interface Tile {
  model: string;
  name: string;
  maker: string | null;
  /** Cheapest first — the order the list arrives in. */
  offers: ProviderOption[];
}

function tilesOf(rows: ProviderOption[]): Tile[] {
  const out: Tile[] = [];
  const seen = new Map<string, Tile>();
  for (const r of rows) {
    let t = seen.get(r.model);
    if (!t) {
      const d = describe(r.model);
      t = { model: r.model, name: d.name, maker: d.maker, offers: [] };
      seen.set(r.model, t);
      out.push(t);
    }
    t.offers.push(r);
  }
  return out;
}

export function ModelTiles({
  kind,
  rows,
  value,
  onChange,
}: {
  kind: JobKind;
  /** Every provider offering every model of this kind, cheapest first. */
  rows: ProviderOption[];
  /** The chosen model (and provider, if pinned), or null for "let the app decide". */
  value: ProviderOption | null;
  onChange: (choice: ProviderOption | null) => void;
}) {
  const [query, setQuery] = useState("");
  const [showProviders, setShowProviders] = useState(false);

  const tiles = useMemo(() => tilesOf(rows), [rows]);
  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return tiles;
    const words = q.split(/\s+/);
    return tiles.filter((t) => {
      const haystack = `${searchTerms(t.model)} ${t.offers[0].unpriced || t.offers[0].price <= 0 ? "free" : ""} ${t.offers
        .map((o) => o.peer_label)
        .join(" ")}`.toLowerCase();
      return words.every((w) => haystack.includes(w));
    });
  }, [tiles, query]);

  // A choice whose provider has gone offline is stale; say so rather than
  // failing at send time.
  const stale =
    value !== null &&
    rows.length > 0 &&
    !rows.some((r) => r.peer_id === value.peer_id && r.model === value.model);

  const noun = kind === "image" ? "pictures" : kind === "video" ? "videos" : "replies";
  const auto = tiles[0];

  return (
    <aside className="model-rail" aria-label="Choose a model">
      <div className="model-rail-head">
        <div className="model-rail-title">Model</div>
        {tiles.length > 6 && (
          <input
            className="model-rail-search"
            value={query}
            placeholder="Find a model…"
            onChange={(e) => setQuery(e.target.value)}
          />
        )}
      </div>

      <div className="model-rail-tiles">
        {tiles.length === 0 ? (
          <div className="model-rail-empty">Nobody is making {noun} right now.</div>
        ) : (
          <>
            {!query && (
              <button
                className={`model-tile auto${value === null ? " active" : ""}`}
                onClick={() => onChange(null)}
              >
                <span className="n">Auto</span>
                <span className="d">
                  {auto ? `Cheapest on offer — ${auto.name} right now` : "Cheapest on offer"}
                </span>
              </button>
            )}
            {matches.length === 0 && (
              <div className="model-rail-empty">Nothing matches “{query}”.</div>
            )}
            {matches.map((t) => {
              const cheapest = t.offers[0];
              const active = value?.model === t.model;
              const free = cheapest.unpriced || cheapest.price <= 0;
              return (
                <div key={t.model} className={`model-tile-wrap${active ? " active" : ""}`}>
                  <button
                    className={`model-tile${active ? " active" : ""}${active && stale ? " stale" : ""}`}
                    onClick={() => {
                      onChange({ ...cheapest, pinned: false });
                      setShowProviders(false);
                    }}
                    title={t.model}
                  >
                    <span className="n">{t.name}</span>
                    {t.maker && <span className="d">{t.maker}</span>}
                    <span className={`s${free ? " free" : ""}`}>{priceLabel(cheapest)}</span>
                  </button>
                  {active && (
                    <div className="model-tile-more">
                      {active && stale && (
                        <div className="model-tile-stale">That provider has gone offline.</div>
                      )}
                      {t.offers.length > 1 ? (
                        <button
                          className="model-tile-link"
                          onClick={() => setShowProviders((v) => !v)}
                        >
                          {value?.pinned
                            ? `Pinned to ${value.peer_label}`
                            : `${t.offers.length} providers`}
                          {showProviders ? " ▾" : " ▸"}
                        </button>
                      ) : (
                        <div className="model-tile-link quiet">Served by {cheapest.peer_label}</div>
                      )}
                      {showProviders &&
                        t.offers.map((o) => (
                          <button
                            key={o.peer_id}
                            className={`model-offer${
                              value?.pinned && value.peer_id === o.peer_id ? " active" : ""
                            }`}
                            onClick={() => onChange({ ...o, pinned: true })}
                          >
                            <span className="p">{o.peer_label}</span>
                            <span className="s">
                              {priceLabel(o)}
                              {o.latency_ms !== null ? ` · ${o.latency_ms} ms` : ""}
                            </span>
                          </button>
                        ))}
                      {value?.pinned && (
                        <button
                          className="model-tile-link"
                          onClick={() => onChange({ ...cheapest, pinned: false })}
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
      {tiles.length > 1 && (
        <div className="model-rail-foot">Cheapest first. Prices are what each operator claims.</div>
      )}
    </aside>
  );
}
