// What the window says about itself, written into the app's log file.
//
// A page that never draws is invisible from the Rust side: the webview
// reports "load finished" whether or not a single React component mounted.
// So the page narrates its own boot — script ran, React mounted, first
// answer from the backend — and anything it throws is sent to the same log
// the backend writes, and drawn on screen instead of leaving it blank.

import { invoke } from "@tauri-apps/api/core";

type Level = "error" | "warn" | "info" | "debug";

const started = performance.now();

function stamp(): string {
  return `+${Math.round(performance.now() - started)}ms`;
}

/** Write one line to the log file (and the devtools console). Never throws. */
export function diag(level: Level, message: string): void {
  const line = `${stamp()} ${message}`;
  try {
    const say = level === "error" ? console.error : level === "warn" ? console.warn : console.log;
    say(`[rootmode] ${line}`);
  } catch {
    // A console that refuses is not our problem.
  }
  try {
    void invoke("client_log", { level, message: line }).catch(() => undefined);
  } catch {
    // No IPC at all — the shell will say so in its own log.
  }
}

export function describeError(e: unknown): string {
  if (e instanceof Error) return `${e.name}: ${e.message}${e.stack ? `\n${e.stack}` : ""}`;
  if (typeof e === "string") return e;
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}

/**
 * Put the failure on screen. Only when nothing else is there: an error
 * after the app has drawn is reported, not allowed to paint over it.
 */
export function showFatal(title: string, detail: string): void {
  const root = document.getElementById("root");
  if (!root || root.childElementCount > 0) return;
  const pre = document.createElement("pre");
  pre.style.cssText =
    "margin:0;padding:28px;font:13px/1.5 ui-monospace,Menlo,monospace;white-space:pre-wrap;" +
    "word-break:break-word;color:#e6e6e6;background:#08090a;min-height:100vh;box-sizing:border-box";
  pre.textContent = `rootmode could not start.\n\n${title}\n\n${detail}\n\nThe log file next to the app's data has the rest.`;
  root.replaceChildren(pre);
}

declare global {
  interface Window {
    /** Set once the bundle's own reporting is up, so public/early.js stands down. */
    __rootmodeDiagnostics?: true;
  }
}

let installed = false;

/** Hook the window's error channels and say hello. Call before rendering. */
export function installDiagnostics(): void {
  if (installed) return;
  installed = true;
  window.__rootmodeDiagnostics = true;

  window.addEventListener("error", (event) => {
    const where = event.filename ? ` at ${event.filename}:${event.lineno}:${event.colno}` : "";
    const detail = event.error ? describeError(event.error) : event.message;
    diag("error", `uncaught${where}: ${detail}`);
    showFatal("An error escaped the page.", detail);
  });

  window.addEventListener("unhandledrejection", (event) => {
    diag("error", `unhandled rejection: ${describeError(event.reason)}`);
  });

  const tauri = "__TAURI_INTERNALS__" in window;
  diag(
    "info",
    `script running · tauri bridge ${tauri ? "present" : "MISSING"} · ${window.innerWidth}x${window.innerHeight} @${window.devicePixelRatio} · ${navigator.userAgent}`,
  );
  if (!tauri) {
    showFatal(
      "The page cannot reach the app.",
      "The Tauri bridge (__TAURI_INTERNALS__) is not on the window, so no command can be called.",
    );
  }
}
