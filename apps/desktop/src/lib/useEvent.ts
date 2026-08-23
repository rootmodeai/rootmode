import { useEffect, useRef } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";

/// Subscribe to a Tauri event for the life of the component.
///
/// `listen()` is async, so a bare `useEffect` that awaits it races React
/// StrictMode's mount → cleanup → mount: cleanup runs before the promise
/// resolves, finds nothing to undo, and the throwaway first mount's listener
/// is never removed. Every event after that fires the callback twice — which
/// is exactly what streamed token deltas made visible as "every every word
/// word doubled doubled". Guard with a `cancelled` flag instead: if cleanup
/// beat the promise, unlisten the moment it resolves rather than never.
///
/// `subscribe` should be referentially stable — `events.onJobDelta` and
/// friends already are, since each just wraps `listen()` for one channel.
export function useEvent<T>(subscribe: (cb: (value: T) => void) => Promise<UnlistenFn>, cb: (value: T) => void) {
  const latest = useRef(cb);
  latest.current = cb;

  useEffect(() => {
    let cancelled = false;
    let unlisten: UnlistenFn | undefined;
    subscribe((value) => latest.current(value)).then((u) => {
      if (cancelled) {
        u();
      } else {
        unlisten = u;
      }
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subscribe]);
}
