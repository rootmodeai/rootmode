import { useEffect, useState, type ReactNode } from "react";

/** Hashes and peer ids are shown truncated but always copyable in full. */
export function Hash({ value, chars = 12 }: { value: string; chars?: number }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 1200);
    return () => clearTimeout(t);
  }, [copied]);

  const short = value.length > chars * 2 ? `${value.slice(0, chars)}…${value.slice(-4)}` : value;
  return (
    <button
      className="hash"
      title={`${value}\n(click to copy)`}
      onClick={() => {
        void navigator.clipboard.writeText(value);
        setCopied(true);
      }}
    >
      {copied ? "copied" : short}
    </button>
  );
}

export function StatusDot({ status }: { status: string }) {
  return <span className={`dot ${status}`} aria-hidden />;
}

export function Panel({ title, children, actions }: { title?: string; children: ReactNode; actions?: ReactNode }) {
  return (
    <section className="panel">
      {(title || actions) && (
        <div className="row" style={{ marginBottom: 12 }}>
          {title && <h2 style={{ margin: 0 }}>{title}</h2>}
          <div className="spacer" />
          {actions}
        </div>
      )}
      {children}
    </section>
  );
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="field">
      <span>{label}</span>
      {children}
    </label>
  );
}

export function Notice({ kind = "warn", children }: { kind?: "warn" | "error" | "ok"; children: ReactNode }) {
  return <div className={`notice ${kind === "warn" ? "" : kind}`}>{children}</div>;
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="empty">{children}</div>;
}

export function Progress({ value }: { value: number }) {
  return (
    <div className="progress">
      <div style={{ width: `${Math.round(Math.max(0, Math.min(1, value)) * 100)}%` }} />
    </div>
  );
}

export function ago(unixSeconds: number): string {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unixSeconds));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}
