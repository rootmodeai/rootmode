import { useCallback, useEffect, useState } from "react";
import { StoreProvider, useStore } from "./lib/store";
import { api } from "./lib/api";
import { diag } from "./lib/diag";
import { Boot } from "./screens/Boot";
import { Intro, markIntroSeen } from "./components/Intro";
import { Chat } from "./screens/Chat";
import { Create } from "./screens/Create";
import { Flows } from "./screens/Flows";
import { Network } from "./screens/Network";
import { Connect } from "./screens/Connect";
import { Settings } from "./screens/Settings";
import { Wallet } from "./screens/Wallet";
import type { NetworkStatus, UpdateInfo } from "./lib/types";
import { Glider } from "./components/Glider";
import { ChatIcon, ImagesIcon, VideoIcon, FlowsIcon, ConnectIcon, WalletIcon, SettingsIcon } from "./components/NavIcons";
import { NavModels } from "./components/NavModels";

export type Screen = "chat" | "image" | "video" | "flows" | "network" | "connect" | "wallet" | "settings";

// "Providers" has no tab of its own — it's an advanced, under-the-hood view,
// and a dedicated nav entry for it reads as something everyone is meant to
// visit. The "N providers online" chip at the bottom of the rail is how
// someone who wants it gets there; everyone else never needs to know it
// exists.
const NAV: Array<{ key: Screen; label: string; icon: (props: { size?: number }) => JSX.Element }> = [
  { key: "chat", label: "Text", icon: ChatIcon },
  { key: "image", label: "Images", icon: ImagesIcon },
  { key: "video", label: "Videos", icon: VideoIcon },
  { key: "flows", label: "Flows", icon: FlowsIcon },
  { key: "connect", label: "Use it elsewhere", icon: ConnectIcon },
  { key: "wallet", label: "Wallet", icon: WalletIcon },
  { key: "settings", label: "Settings", icon: SettingsIcon },
];

export default function App() {
  return (
    <StoreProvider>
      <Gate />
    </StoreProvider>
  );
}

/** Nothing is shown until the app knows whether it can do anything. */
function Gate() {
  const { ready, bootError, settings } = useStore();
  const [entered, setEntered] = useState(false);
  // The intro plays over the boot screen the first time this install
  // starts, and never again unless asked for in Settings. Until the
  // settings have been read nothing is decided, so it neither flashes for
  // an old install nor is missed by a new one.
  const [dismissed, setDismissed] = useState(false);
  const intro = settings !== null && !settings.intro_seen && !dismissed;
  // Stable on purpose: Boot restarts its search — and its give-up timer —
  // whenever this changes, and the store re-renders Gate every ten seconds
  // as the peer list is re-read. A fresh closure each time meant the
  // "Continue anyway" button could never arrive.
  const enter = useCallback(() => setEntered(true), []);

  // The page's own account of its boot, for the log file: mounted, got its
  // first answers from the backend, moved past the boot screen.
  useEffect(() => {
    diag("info", "react mounted; boot screen showing");
  }, []);
  useEffect(() => {
    if (ready) diag(bootError ? "error" : "info", `store ready${bootError ? `, boot error: ${bootError}` : ""}`);
  }, [ready, bootError]);
  useEffect(() => {
    if (entered) diag("info", "entered the main shell");
  }, [entered]);

  const film = intro ? (
    <Intro
      onDone={() => {
        void markIntroSeen().catch(() => undefined);
        setDismissed(true);
      }}
    />
  ) : null;
  if (!ready || !entered) {
    return (
      <>
        <Boot onReady={enter} />
        {film}
      </>
    );
  }
  return (
    <>
      <Shell />
      {film}
    </>
  );
}

function Shell() {
  const [screen, setScreen] = useState<Screen>("chat");
  // The section whose models are unfolded in the navigation. Closed until
  // you open one; clicking the open section again folds it away.
  const [unfolded, setUnfolded] = useState<Screen | null>(null);
  const { peers, bootError } = useStore();
  const [status, setStatus] = useState<NetworkStatus | null>(null);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);

  // Cheap and steady: the rail's status is the one thing on screen at all
  // times, so it should never be stale by more than a few seconds.
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      try {
        const s = await api.networkStatus();
        if (!cancelled) setStatus(s);
      } catch {
        // Nothing to say; try again shortly.
      }
      if (!cancelled) setTimeout(tick, 5000);
    };
    void tick();
    return () => {
      cancelled = true;
    };
  }, [peers.length]);

  useEffect(() => {
    const t = setTimeout(() => {
      void api.checkUpdate().then((u) => {
        if (u.available) setUpdate(u);
      }).catch(() => undefined);
    }, 2500);
    return () => clearTimeout(t);
  }, []);

  const online = status?.online ?? 0;

  return (
    <div className="app">
      <aside className="rail">
        <div className="brand">
          <span className="brand-mark">
            <Glider size={18} />
          </span>
          rootmode
        </div>

        <nav className="nav">
          {NAV.map((item) => {
            const work = item.key === "chat" || item.key === "image" || item.key === "video";
            const open = work && unfolded === item.key;
            return (
              <div key={item.key} className={`nav-section${open ? " open" : ""}`}>
                <button
                  aria-current={screen === item.key}
                  aria-expanded={work ? open : undefined}
                  onClick={() => {
                    setScreen(item.key);
                    if (work) setUnfolded(open && screen === item.key ? null : item.key);
                  }}
                >
                  <span className="icon">
                    <item.icon />
                  </span>
                  {item.label}
                  {work && <span className="nav-caret">{open ? "▾" : "▸"}</span>}
                </button>
                {open && (
                  <NavModels
                    kind={item.key === "chat" ? "llm" : item.key === "image" ? "image" : "video"}
                    onPick={() => setScreen(item.key)}
                  />
                )}
              </div>
            );
          })}
        </nav>

        <div className="rail-foot">
          <button className="status-chip" onClick={() => setScreen("network")}>
            <span className={`dot ${online > 0 ? "ok" : status?.searching ? "busy" : "off"}`} />
            <span>
              {online > 0
                ? `${online} provider${online === 1 ? "" : "s"} online`
                : status?.searching
                  ? "Looking for providers…"
                  : "Nobody online"}
            </span>
          </button>
        </div>
      </aside>

      <main className="main">
        {update?.available && update.latest && (
          <div className="note update-bar">
            <span>
              {update.latest} is out. You have {update.current}.
            </span>
            <button
              className="btn primary sm"
              onClick={() => void api.openUpdate(update.url)}
            >
              Download
            </button>
            <button
              className="btn sm"
              onClick={() => {
                if (update.latest) void api.skipUpdate(update.latest);
                setUpdate(null);
              }}
            >
              Later
            </button>
          </div>
        )}
        {bootError && (
          <div className="page">
            <div className="note bad">Could not start: {bootError}</div>
          </div>
        )}
        {/* Hidden, not unmounted. A tab you look away from is still a place
            you were: the chat you had open, how far you had scrolled, the
            half-typed message and the answer arriving as you read it all
            belong to the screen, and throwing them away because somebody
            glanced at another tab is the app forgetting on your behalf.
            Only the heavy screens are kept alive — the settings pages have
            nothing worth preserving. */}
        <div className="screen" hidden={screen !== "chat"}>
          <Chat />
        </div>
        <div className="screen" hidden={screen !== "image"}>
          <Create kind="image" />
        </div>
        <div className="screen" hidden={screen !== "video"}>
          <Create kind="video" />
        </div>
        <div className="screen" hidden={screen !== "flows"}>
          <Flows />
        </div>
        {screen === "network" && <Network />}
        {screen === "connect" && <Connect />}
        {screen === "wallet" && <Wallet />}
        {screen === "settings" && <Settings />}
      </main>
    </div>
  );
}
