import { useSyncExternalStore } from "react";
import type { JobKind, ProviderOption } from "./types";

/**
 * Which model is chosen for each kind of work.
 *
 * Chosen in the navigation, used by the screen: the two are far apart in
 * the tree and both need the same answer, so it lives here rather than in
 * either. `null` means "let the app decide".
 */
const chosen: Record<JobKind, ProviderOption | null> = { llm: null, image: null, video: null };
/** Bumped when a model is picked from the navigation, so the screen it
 * opens can put the cursor in the message box. */
const picks: Record<JobKind, number> = { llm: 0, image: 0, video: 0 };
const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function setChoice(kind: JobKind, value: ProviderOption | null, fromNav = false) {
  chosen[kind] = value;
  if (fromNav) picks[kind] += 1;
  listeners.forEach((l) => l());
}

export function useChoice(kind: JobKind): [ProviderOption | null, (v: ProviderOption | null) => void] {
  const value = useSyncExternalStore(subscribe, () => chosen[kind]);
  return [value, (v) => setChoice(kind, v)];
}

/** Changes each time a model is picked from the navigation for this kind. */
export function usePick(kind: JobKind): number {
  return useSyncExternalStore(subscribe, () => picks[kind]);
}
