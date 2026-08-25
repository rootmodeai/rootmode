import { api, errorText } from "../lib/api";
import type { FundingKind, PotCheck } from "../lib/types";

export function usd(micros: number) {
  return `$${(micros / 1_000_000).toFixed(2)}`;
}

/** Map a job or check error onto a funding kind, or null if it is unrelated. */
export function fundingKindFromText(text: string): FundingKind | null {
  if (/limit for a single job|per-job cap|prepaid budget|raise (the |that )?limit|raise the cap/i.test(text)) {
    return "cap";
  }
  if (/needs a little ETH|payout address/i.test(text)) {
    return "chain";
  }
  if (/does not cover|deposit more|fund your pot|this provider charges|could not lock funds/i.test(text)) {
    return "empty";
  }
  if (/local chain|not running|\.\/contracts\/local|can't reach base|settlement is not configured/i.test(text)) {
    return "chain";
  }
  return null;
}

function title(kind: FundingKind): string {
  switch (kind) {
    case "cap":
      return "This reply hit your spending limit";
    case "empty":
      return "Your wallet needs more USDC";
    case "chain":
      return "Can't reach the network";
    default:
      return "";
  }
}

/**
 * Shown in chat when a priced job cannot run, or when it stops because
 * the per-job ceiling was reached. The button matches the problem: a cap
 * is raised in Wallet, an empty balance is funded there.
 */
export function FundingNotice({
  kind,
  reason,
  capMicros,
  onActionError,
}: {
  kind: Exclude<FundingKind, "ok">;
  reason: string;
  capMicros?: number;
  onActionError?: (msg: string) => void;
}) {
  const cap = capMicros && capMicros > 0 ? usd(capMicros) : null;
  return (
    <div className="funding-card">
      <div className="funding-title">{title(kind)}</div>
      <div className="funding-body">{reason}</div>
      {kind === "cap" && cap && (
        <div className="funding-meta">
          A single reply cannot spend more than {cap}. Raise “Max per job” in
          Wallet if you want longer answers.
        </div>
      )}
      {kind !== "chain" && (
        <button
          className="btn primary"
          style={{ marginTop: 12 }}
          onClick={() =>
            void api.potOpenFund().catch((e) => onActionError?.(errorText(e)))
          }
        >
          {kind === "cap" ? "Raise the limit in MetaMask" : "Deposit in MetaMask"}
        </button>
      )}
    </div>
  );
}

/** Quiet line under the composer when a priced model is on a capped pot. */
export function FundingHint({ capMicros }: { capMicros: number }) {
  if (capMicros <= 0) return null;
  return (
    <div className="funding-hint">
      Spending limit {usd(capMicros)} per reply
    </div>
  );
}

export function noticeFromCheck(check: PotCheck): {
  kind: Exclude<FundingKind, "ok">;
  reason: string;
  capMicros: number;
} | null {
  if (check.ready || check.kind === "ok") return null;
  const kind = (["cap", "empty", "chain"] as const).includes(
    check.kind as "cap" | "empty" | "chain",
  )
    ? (check.kind as "cap" | "empty" | "chain")
    : check.needs_fund
      ? "empty"
      : "cap";
  return { kind, reason: check.reason, capMicros: check.cap_micros };
}
