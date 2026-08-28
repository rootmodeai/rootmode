// Single shared store. Jobs stream in as backend events, so every screen sees
// the same state without polling.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { api, errorText, events } from "./api";
import { diag } from "./diag";
import { useEvent } from "./useEvent";
import type {
  JobDelta,
  JobRecord,
  Peer,
  PublicIdentity,
  ResultRecord,
  Settings,
} from "./types";

/** An answer as far as it has been written, for a job still writing it. */
export interface Stream {
  text: string;
  thinking: string;
}

interface Store {
  identity: PublicIdentity | null;
  peers: Peer[];
  jobs: JobRecord[];
  settings: Settings | null;
  ready: boolean;
  bootError: string | null;
  refreshPeers: () => Promise<void>;
  refreshJobs: () => Promise<void>;
  refreshIdentity: () => Promise<void>;
  setSetting: (key: string, value: string) => Promise<void>;
  openJobs: number;
  /// Partial answers, by job id. Deltas arrive whether or not the screen that
  /// asked for them is mounted, so they are accumulated here: a chat left
  /// half-written and come back to shows everything said while you were
  /// away, not just the tokens since you returned.
  streams: Record<string, Stream>;
  clearStream: (jobId: string) => void;
}

const Ctx = createContext<Store | null>(null);

export function StoreProvider({ children }: { children: ReactNode }) {
  const [identity, setIdentity] = useState<PublicIdentity | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [jobs, setJobs] = useState<JobRecord[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [ready, setReady] = useState(false);
  const [bootError, setBootError] = useState<string | null>(null);
  const [streams, setStreams] = useState<Record<string, Stream>>({});

  const clearStream = useCallback((jobId: string) => {
    setStreams((prev) => {
      if (!(jobId in prev)) return prev;
      const next = { ...prev };
      delete next[jobId];
      return next;
    });
  }, []);

  const refreshPeers = useCallback(async () => setPeers(await api.listPeers()), []);
  const refreshJobs = useCallback(async () => setJobs(await api.listJobs(200)), []);
  const refreshIdentity = useCallback(async () => setIdentity(await api.getIdentity()), []);

  const setSetting = useCallback(async (key: string, value: string) => {
    setSettings(await api.setSetting(key, value));
  }, []);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [id, p, j, s] = await Promise.all([
          api.getIdentity(),
          api.listPeers(),
          api.listJobs(200),
          api.getSettings(),
        ]);
        if (cancelled) return;
        setIdentity(id);
        setPeers(p);
        setJobs(j);
        setSettings(s);
        diag(
          "info",
          `first backend answers: peer ${id.peer_id.slice(0, 12)}…, ${p.length} peer(s), ${j.length} job(s), theme ${s.theme ?? "default"}`,
        );
      } catch (e) {
        if (!cancelled) {
          setBootError(errorText(e));
          diag("error", `first backend calls failed: ${errorText(e)}`);
        }
      } finally {
        if (!cancelled) setReady(true);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEvent(events.onJobUpdate, (job) => {
    // A finished job's buffer has served its purpose — the reply is a row in
    // the database now. The grace period is for the screen that is showing
    // it: dropping it the instant the job ends would blank the answer a beat
    // before the stored one arrives to replace it.
    if (job.status === "done" || job.status === "failed") {
      setTimeout(() => clearStream(job.job_id), 5000);
    }
    setJobs((prev) => {
      const i = prev.findIndex((j) => j.job_id === job.job_id);
      if (i === -1) return [job, ...prev];
      const next = prev.slice();
      next[i] = job;
      return next;
    });
  });

  useEvent(events.onJobDelta, (delta: JobDelta) => {
    setStreams((prev) => {
      const at = prev[delta.job_id];
      return {
        ...prev,
        [delta.job_id]: {
          text: (at?.text ?? "") + (delta.text ?? ""),
          thinking: (at?.thinking ?? "") + (delta.thinking ?? ""),
        },
      };
    });
  });

  useEvent(events.onPeerUpdate, (peer) => {
    setPeers((prev) => {
      const i = prev.findIndex((p) => p.id === peer.id);
      // A peer we have never seen is the whole point of discovery; mapping
      // over the list silently dropped it, so the screen stayed empty while
      // the backend knew perfectly well it was there.
      if (i === -1) return [...prev, peer];
      const next = prev.slice();
      next[i] = peer;
      return next;
    });
  });

  // Peers also disappear — pruned once they stop answering — and no event
  // carries that. A periodic read of a local table is cheap and self-heals in
  // both directions.
  useEffect(() => {
    const timer = setInterval(() => {
      void api
        .listPeers()
        .then(setPeers)
        .catch(() => undefined);
    }, 10_000);
    return () => clearInterval(timer);
  }, []);

  // The theme lives on <html> so the very first paint is right.
  useEffect(() => {
    document.documentElement.dataset.theme = settings?.theme === "dark" ? "dark" : "light";
  }, [settings?.theme]);

  const openJobs = useMemo(
    () => jobs.filter((j) => j.status === "queued" || j.status === "running").length,
    [jobs],
  );

  const value: Store = {
    identity,
    peers,
    jobs,
    settings,
    ready,
    bootError,
    refreshPeers,
    refreshJobs,
    refreshIdentity,
    setSetting,
    openJobs,
    streams,
    clearStream,
  };

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useStore(): Store {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useStore outside StoreProvider");
  return ctx;
}

/** Result for one job, loaded on demand (images come back as data URLs). */
export function useResult(jobId: string | null) {
  const [result, setResult] = useState<ResultRecord | null>(null);
  const [image, setImage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setResult(null);
    setImage(null);
    setError(null);
    if (!jobId) return;
    (async () => {
      try {
        const r = await api.getResult(jobId);
        if (cancelled) return;
        setResult(r);
        if (r?.image_path) {
          const dataUrl = await api.readResultImage(jobId);
          if (!cancelled) setImage(dataUrl);
        }
      } catch (e) {
        if (!cancelled) setError(errorText(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [jobId]);

  return { result, image, error };
}
