import { useEffect, useMemo, useRef, useState } from "react";
// import { flagOf, countryName } from "../lib/country";   // see below
import { api } from "../lib/api";
import type { JobKind, ProviderOption } from "../lib/types";

/**
 * Choosing who runs your work, by hand.
 *
 * The app can pick for you — cheapest provider serving a model, latency
 * breaking ties — and does when you say nothing. This is the other case: you
 * want to look. So it lists every provider offering every model rather than
 * collapsing to the best one, because a list that hides the dearer provider
 * is the app deciding while pretending to ask.
 *
 * Search matches model *or* provider, so "deepseek" finds everyone serving it
 * and "sparky" finds everything one machine offers. Cheapest first throughout,
 * which is the ordering a price is for.
 */
export function ProviderPicker({
  kind,
  value,
  onChange,
}: {
  kind: JobKind;
  /** The chosen pair, or null for "let the app decide". */
  value: ProviderOption | null;
  onChange: (choice: ProviderOption | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [rows, setRows] = useState<ProviderOption[]>([]);
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

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return rows;
    // Every word has to appear somewhere, so "deepseek spark" narrows rather
    // than widening the way an any-word match would.
    const words = q.split(/\s+/);
    return rows.filter((r) => {
      const haystack =
        // Country stays searchable by code even while the flag is hidden:
        // typing "de" costs nothing and matching on it surprises nobody.
        `${r.model} ${r.peer_label} ${r.peer_country ?? ""}`.toLowerCase();
      return words.every((w) => haystack.includes(w));
    });
  }, [rows, query]);

  // A choice whose provider has gone offline is stale; say so rather than
  // failing at send time.
  const stale =
    value !== null &&
    rows.length > 0 &&
    !rows.some((r) => r.peer_id === value.peer_id && r.model === value.model);

  const label = value
    ? `${value.model} · ${value.peer_label}`
    : rows.length > 0
      ? `Cheapest — ${rows[0].model} · ${rows[0].peer_label}`
      : "Nobody online";

  return (
    <div className="picker" ref={boxRef}>
      <button
        className={`picker-button${stale ? " stale" : ""}`}
        onClick={() => setOpen((v) => !v)}
        title={stale ? "That provider has gone offline" : "Choose a model and provider"}
      >
        <span className="picker-label">{label}</span>
        <span className="picker-caret">▾</span>
      </button>

      {open && (
        <div className="picker-menu">
          <input
            ref={searchRef}
            className="picker-search"
            value={query}
            placeholder="Search a model or a provider…"
            onChange={(e) => setQuery(e.target.value)}
          />

          <div className="picker-rows">
            <button
              className={`picker-row${value === null ? " active" : ""}`}
              onClick={() => {
                onChange(null);
                setOpen(false);
              }}
            >
              <span className="m">Let rootmode choose</span>
              <span className="s">cheapest, then fastest</span>
            </button>

            {matches.length === 0 ? (
              <div className="empty" style={{ padding: "14px 10px", fontSize: 12.5 }}>
                {rows.length === 0
                  ? "Nobody is offering anything right now."
                  : `Nothing matches “${query}”.`}
              </div>
            ) : (
              matches.map((r) => (
                <button
                  key={`${r.peer_id}:${r.model}`}
                  className={`picker-row${
                    value?.peer_id === r.peer_id && value?.model === r.model ? " active" : ""
                  }`}
                  onClick={() => {
                    onChange(r);
                    setOpen(false);
                  }}
                >
                  <span className="m">{r.model}</span>
                  <span className="p">
                    {/* Flags are off while the network runs on our own
                        capacity — see Network.tsx.

                    {flagOf(r.peer_country) && (
                      <span title={`Says it is in ${countryName(r.peer_country)}`}>
                        {flagOf(r.peer_country)}{" "}
                      </span>
                    )} */}
                    {r.peer_label}
                  </span>
                  <span className="s">
                    {r.unpriced ? "free" : `${r.price.toFixed(2)} ${r.currency}`}
                    {r.latency_ms !== null ? ` · ${r.latency_ms} ms` : ""}
                  </span>
                </button>
              ))
            )}
          </div>

          {matches.length > 1 && (
            <div className="picker-foot">Cheapest first. Prices are what each operator claims.</div>
          )}
        </div>
      )}
    </div>
  );
}
