import { useEffect, useState } from "react";
import { api, errorText } from "../lib/api";
import { diag } from "../lib/diag";
import type { NetworkStatus } from "../lib/types";
import { Glider } from "../components/Glider";

/**
 * What you see while the app joins the network.
 *
 * It is honest about how long this takes and it never traps you: after a few
 * seconds of finding nobody you can carry on anyway, because a network with
 * nobody on it is a normal thing to encounter and not an error.
 */

type Stage = "starting" | "searching" | "found" | "empty";

const GIVE_UP_AFTER = 12_000;

export function Boot({ onReady }: { onReady: () => void }) {
  const [stage, setStage] = useState<Stage>("starting");
  const [status, setStatus] = useState<NetworkStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    const started = Date.now();
    // Each outcome is logged once: the first answer, the first refusal, and
    // giving up. Every 700ms tick after that would only bury them.
    let loggedAnswer = false;
    let loggedFailure = false;
    let loggedGiveUp = false;

    const tick = async () => {
      try {
        const s = await api.networkStatus();
        if (cancelled) return;
        setStatus(s);
        if (!loggedAnswer) {
          loggedAnswer = true;
          diag("info", `first network status: online=${s.online} searching=${String(s.searching)}`);
        }

        if (s.online > 0) {
          setStage("found");
          diag("info", `found ${s.online} provider(s); leaving the boot screen`);
          // Let the tick land visibly rather than flashing past it.
          setTimeout(() => !cancelled && onReady(), 650);
          return;
        }

        const gaveUp = Date.now() - started > GIVE_UP_AFTER;
        if (gaveUp && !loggedGiveUp) {
          loggedGiveUp = true;
          diag("warn", `no providers after ${GIVE_UP_AFTER}ms; offering to continue anyway`);
        }
        setStage(gaveUp ? "empty" : "searching");
      } catch (e) {
        // The backend is still starting; the next tick will do.
        if (!loggedFailure) {
          loggedFailure = true;
          diag("warn", `network status failed: ${errorText(e)}`);
        }
      }
      if (!cancelled) setTimeout(tick, 700);
    };

    setTimeout(tick, 400);
    return () => {
      cancelled = true;
    };
  }, [onReady]);

  const peers = status?.online ?? 0;

  return (
    <div className="boot">
      <div className="boot-inner">
        <div className="boot-mark">
          <Glider size={42} animate />
        </div>
        <h1>rootmode</h1>
        <p>Connecting you to the network</p>

        <div className="boot-steps">
          <Step done label="Loaded your keys" />
          <Step
            done={stage !== "starting"}
            active={stage === "starting"}
            label="Joining the network"
          />
          <Step
            done={stage === "found"}
            active={stage === "searching"}
            label={
              stage === "found"
                ? `Found ${peers} ${peers === 1 ? "provider" : "providers"}`
                : "Looking for providers"
            }
          />
        </div>

        {stage === "empty" && (
          <>
            <div className="note" style={{ marginTop: 22, textAlign: "left" }}>
              No providers found yet. Nobody may be online right now, or this
              computer may not be able to reach them.
            </div>
            <div className="boot-actions">
              <button className="btn primary" onClick={onReady}>
                Continue anyway
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function Step({ label, active, done }: { label: string; active?: boolean; done?: boolean }) {
  return (
    <div className={`boot-step ${done ? "done" : active ? "active" : ""}`}>
      <span className="boot-dot" />
      <span>{label}</span>
    </div>
  );
}
