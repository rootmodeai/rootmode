import { useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { api } from "../lib/api";
import { diag } from "../lib/diag";

/** Remember, with this install's own data, that the film has played. */
export function markIntroSeen(seen = true): Promise<void> {
  return api.setSetting("intro_seen", String(seen)).then(() => undefined);
}

/**
 * The first thing a new install sees: fifteen seconds, full window, over
 * the boot screen — which carries on joining the network underneath, so
 * the film costs no time. Anything that stops it playing (an engine
 * without the codec, a policy that blocks autoplay, a stall) skips it
 * rather than leaving a black window; so does the Skip button.
 */
export function Intro({ onDone }: { onDone: () => void }) {
  const ref = useRef<HTMLVideoElement | null>(null);
  const [leaving, setLeaving] = useState(false);
  const [src, setSrc] = useState<string | null>(null);
  const done = useRef(false);

  const finish = () => {
    if (done.current) return;
    done.current = true;
    setLeaving(true);
    setTimeout(onDone, 350);
  };

  // The film ships as a bundle resource and is played through the asset
  // protocol, which serves byte ranges; the page's own origin does not, and
  // a video element given a URL there loads forever.
  useEffect(() => {
    api
      .introPath()
      .then((p) => {
        if (!p) {
          diag("warn", "intro: no film shipped; skipping");
          finish();
          return;
        }
        setSrc(convertFileSrc(p));
      })
      .catch((e) => {
        diag("warn", `intro: cannot locate the film: ${String(e)}`);
        finish();
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const v = ref.current;
    if (!v || !src) return;
    // With sound where the engine allows it; muted where it does not,
    // rather than not at all.
    const start = async () => {
      try {
        v.muted = false;
        await v.play();
        diag("info", "intro: playing with sound");
      } catch (e) {
        diag("warn", `intro: play with sound refused: ${String(e)}`);
        try {
          v.muted = true;
          await v.play();
          diag("info", "intro: playing muted");
        } catch (e2) {
          diag("warn", `intro: play refused: ${String(e2)}`);
          finish();
        }
      }
    };
    v.addEventListener("error", () => {
      const err = v.error;
      diag("error", `intro: media error code=${err?.code ?? "?"} message=${err?.message ?? ""} networkState=${v.networkState} readyState=${v.readyState}`);
    });
    v.addEventListener("loadedmetadata", () => diag("info", `intro: metadata loaded, duration ${v.duration}s`));
    void start();
    // A film that has not begun within a few seconds is not going to.
    const stall = setTimeout(() => {
      if (v.currentTime === 0) {
        diag("warn", `intro: no progress after 4s (readyState=${v.readyState} networkState=${v.networkState}); skipping`);
        finish();
      }
    }, 4000);
    return () => clearTimeout(stall);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [src]);

  return (
    <div className={`intro${leaving ? " leaving" : ""}`}>
      {src && <video ref={ref} src={src} playsInline preload="auto" onEnded={finish} onError={finish} />}
      <button className="intro-skip" onClick={finish}>
        Skip
      </button>
    </div>
  );
}
