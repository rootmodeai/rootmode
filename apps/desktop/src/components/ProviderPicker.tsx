import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "../lib/api";
import { describe, priceLabel, searchTerms } from "../lib/models";
import type { JobKind, ProviderOption } from "../lib/types";

/**
 * Choosing what answers.
 *
 * You choose a *model* — Nano Banana, Veo, Claude — by the name people use
 * for it, with the catalogue id kept small beside it for anyone who wants
 * it. The app picks the provider: the cheapest, drawn at random among
 * equals so nobody's node takes all the work. Each model's providers are
 * one click away for the other case — you have watched a node time out all
 * morning and would rather pin a different one.
 *
 * Search matches the name, the maker, the id, and the names people use
 * ("nano banana" finds the Gemini image models), and every word has to
 * match, so "google video" narrows rather than widens.
 */

interface ModelRow {
  model: string;
  name: string;
  maker: string | null;
  kind: JobKind;
  /** Cheapest first — the order the list arrives in. */
  offers: ProviderOption[];
}

function byModel(rows: ProviderOption[]): ModelRow[] {
  const out: ModelRow[] = [];
  const seen = new Map<string, ModelRow>();
  for (const r of rows) {
    let g = seen.get(r.model);
    if (!g) {
      const d = describe(r.model);
      g = { model: r.model, name: d.name, maker: d.maker, kind: r.kind, offers: [] };
      seen.set(r.model, g);
      out.push(g);
    }
    g.offers.push(r);
  }
  return out;
}

export function ProviderPicker({
  kind,
  value,
  onChange,
}: {
  kind: JobKind;
  /** The chosen model (and provider, if pinned), or null for "let the app decide". */
  value: ProviderOption | null;
  onChange: (choice: ProviderOption | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [rows, setRows] = useState<ProviderOption[]>([]);
  const [expanded, setExpanded] = useState<string | null>(null);
  const boxRef = useRef<HTMLDivElement | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    const load = () =>
      void api
        .availableProviders(kind)
        .then(setRows)
        .catch(() => undefined);
    load();
    // Providers come and go; a stale list here is a choice that fails on send.
    const t = setInterval(load, 8000);
    return () => clearInterval(t);
  }, [kind]);

  // Clicking away closes it, as any menu should.
  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => {
      if (!boxRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const escape = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);

  useEffect(() => {
    if (open) searchRef.current?.focus();
  }, [open]);

  const models = useMemo(() => byModel(rows), [rows]);

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return models;
    const words = q.split(/\s+/);
    return models.filter((m) => {
      const haystack = `${searchTerms(m.model)} ${m.offers
        .map((o) => `${o.peer_label} ${o.peer_country ?? ""}`)
        .join(" ")}`.toLowerCase();
      return words.every((w) => haystack.includes(w));
    });
  }, [models, query]);

  // A choice whose provider has gone offline is stale; say so rather than
  // failing at send time.
  const stale =
    value !== null &&
    rows.length > 0 &&
    !rows.some((r) => r.peer_id === value.peer_id && r.model === value.model);

  const shown = value ?? rows[0] ?? null;
  const current = shown ? describe(shown.model) : null;
  const detail = shown
    ? [
        current?.maker,
        value?.pinned ? value.peer_label : null,
        priceLabel(shown),
      ]
        .filter(Boolean)
        .join(" · ")
    : "";

  return (
    <div className="picker" ref={boxRef}>
      <button
        className={`picker-button${stale ? " stale" : ""}`}
        onClick={() => setOpen((v) => !v)}
        title={stale ? "That provider has gone offline" : "Choose a model"}
      >
        <span className="picker-kicker">{value ? "Model" : "Model · auto"}</span>
        <span className="picker-name">{current ? current.name : "Nobody online"}</span>
        {detail && <span className="picker-detail">{detail}</span>}
        <span className="picker-caret">▾</span>
      </button>

      {open && (
        <div className="picker-menu">
          <input
            ref={searchRef}
            className="picker-search"
            value={query}
            placeholder="Search a model — Nano Banana, Veo, Claude…"
            onChange={(e) => setQuery(e.target.value)}
          />

          <div className="picker-rows">
            <button
              className={`picker-row picker-auto${value === null ? " active" : ""}`}
              onClick={() => {
                onChange(null);
                setOpen(false);
              }}
            >
              <span className="n">Let rootmode choose</span>
              <span className="d">the cheapest model on offer, from whoever serves it cheapest</span>
            </button>

            {matches.length === 0 ? (
              <div className="empty" style={{ padding: "14px 10px", fontSize: 12.5 }}>
                {rows.length === 0
                  ? "Nobody is offering anything right now."
                  : `Nothing matches “${query}”.`}
              </div>
            ) : (
              matches.map((m) => {
                const cheapest = m.offers[0];
                const active = value?.model === m.model;
                const isOpen = expanded === m.model;
                return (
                  <div className={`picker-model${active ? " active" : ""}`} key={m.model}>
                    <button
                      className="picker-row"
                      onClick={() => {
                        onChange({ ...cheapest, pinned: false });
                        setOpen(false);
                      }}
                    >
                      <span className="n">{m.name}</span>
                      <span className="d">
                        {[m.maker, m.name !== m.model ? m.model : null]
                          .filter(Boolean)
                          .join(" · ")}
                      </span>
                      <span className="s">
                        {priceLabel(cheapest)}
                        {m.offers.length > 1
                          ? ` · ${m.offers.length} providers`
                          : ` · ${cheapest.peer_label}`}
                      </span>
                    </button>
                    {m.offers.length > 1 && (
                      <button
                        className="picker-expand"
                        title={isOpen ? "Hide providers" : "Choose the provider yourself"}
                        onClick={() => setExpanded(isOpen ? null : m.model)}
                      >
                        {isOpen ? "▾" : "▸"}
                      </button>
                    )}
                    {isOpen &&
                      m.offers.map((o) => (
                        <button
                          key={o.peer_id}
                          className={`picker-offer${
                            value?.pinned && value.peer_id === o.peer_id && value.model === o.model
                              ? " active"
                              : ""
                          }`}
                          onClick={() => {
                            onChange({ ...o, pinned: true });
                            setOpen(false);
                          }}
                        >
                          <span className="p">{o.peer_label}</span>
                          <span className="s">
                            {priceLabel(o)}
                            {o.latency_ms !== null ? ` · ${o.latency_ms} ms` : ""}
                          </span>
                        </button>
                      ))}
                  </div>
                );
              })
            )}
          </div>

          {matches.length > 1 && (
            <div className="picker-foot">
              Cheapest first. Prices are what each operator claims.
            </div>
          )}
        </div>
      )}
    </div>
  );
}
