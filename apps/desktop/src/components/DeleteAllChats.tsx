import { useState } from "react";
import { api, errorText } from "../lib/api";
import { useStore } from "../lib/store";
import { SkullIcon } from "./NavIcons";

/**
 * The one control that empties everything.
 *
 * It sits under the list rather than beside a chat, because "all of them" is
 * a different kind of act from "this one" and shouldn't be a neighbour of the
 * per-row delete. It asks first, in place, and says plainly what goes: chats,
 * pictures and videos, in every tab — not just the tab you happen to be on.
 */
export function DeleteAllChats({ onDone }: { onDone: () => void }) {
  const { refreshJobs } = useStore();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function wipe() {
    setBusy(true);
    setError(null);
    try {
      // One backend call rather than a loop of deletes: it erases the files
      // first, then clears chats, messages, jobs and results in a single
      // transaction, so there is no window where half the history is gone.
      await api.deleteAllConversations();
      // The jobs those chats ran are gone from the database too, so the
      // shared list has to be re-read or the rail keeps counting work that no
      // longer exists.
      await refreshJobs();
      setConfirming(false);
      onDone();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  if (!confirming) {
    return (
      <div className="chat-list-foot">
        <button className="wipe-all" onClick={() => setConfirming(true)}>
          <SkullIcon size={18} />
          <span>Delete all chats</span>
        </button>
      </div>
    );
  }

  return (
    <div className="chat-list-foot">
      <div className="wipe-confirm">
        <div className="t">Delete everything?</div>
        <div style={{ fontSize: 11.5, color: "var(--text-3)", marginTop: 4 }}>
          Every chat, every picture and video they made, and the records behind them.
          The files are erased, not just forgotten.
        </div>
        {error && (
          <div className="note bad" style={{ marginTop: 6, fontSize: 11.5 }}>
            {error}
          </div>
        )}
        <div style={{ display: "grid", gap: 6, marginTop: 10 }}>
          <button className="wipe-all" disabled={busy} onClick={() => void wipe()}>
            <SkullIcon size={18} />
            <span>{busy ? "Deleting…" : "Yes, delete everything"}</span>
          </button>
          <button
            className="btn sm ghost"
            style={{ width: "100%" }}
            disabled={busy}
            onClick={() => setConfirming(false)}
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
