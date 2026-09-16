import { useSyncExternalStore } from "react";

/**
 * Which screen is showing.
 *
 * Chosen in the rail, used by the hint under the composer that sends you to
 * Wallet — the two are far apart in the tree, so the current screen lives
 * here rather than in either.
 */
export type Screen =
  | "chat"
  | "image"
  | "video"
  | "flows"
  | "network"
  | "connect"
  | "wallet"
  | "settings";

let current: Screen = "chat";
const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function goTo(screen: Screen) {
  if (current === screen) return;
  current = screen;
  listeners.forEach((l) => l());
}

export function useScreen(): Screen {
  return useSyncExternalStore(subscribe, () => current);
}
