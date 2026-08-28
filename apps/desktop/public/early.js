// The first script on the page, and the only one written to run anywhere.
//
// It is plain ES5 on purpose. The app bundle is checked by the engine as a
// whole before any of it runs, so one construct an older WebKit does not
// know — a regex form, a syntax it never learned — throws it away in
// silence: a blank window, and no code left to say why. This runs first,
// asks for nothing modern, and when the bundle fails to start it writes
// the reason on screen and into the app's log through the Tauri bridge.
//
// If the bundle does start, it takes over error reporting and this one
// stands down (see src/lib/diag.ts).
(function () {
  var reported = false;

  function log(level, message) {
    try {
      var bridge = window.__TAURI_INTERNALS__;
      if (bridge && typeof bridge.invoke === "function") {
        bridge.invoke("client_log", { level: level, message: "early: " + message });
      }
    } catch (e) {
      // No bridge, nothing to tell.
    }
  }

  function show(message) {
    var root = document.getElementById("root");
    if (!root || root.childElementCount > 0) return;
    var pre = document.createElement("pre");
    pre.style.cssText =
      "margin:0;padding:28px;font:13px/1.5 Menlo,monospace;white-space:pre-wrap;" +
      "word-break:break-word;color:#e6e6e6;background:#08090a;min-height:100vh;box-sizing:border-box";
    pre.textContent =
      "rootmode could not start on this computer.\n\n" +
      message +
      "\n\nThis usually means the system's web engine is too old for this version of the app. " +
      "The log file in the app's data folder has the details.";
    root.appendChild(pre);
  }

  window.addEventListener("error", function (event) {
    if (window.__rootmodeDiagnostics) return; // the bundle is up; it reports
    var where = event.filename ? " at " + event.filename + ":" + event.lineno + ":" + event.colno : "";
    var detail = event.message || "unknown error";
    if (event.error && event.error.stack) detail += "\n" + event.error.stack;
    log("error", "uncaught" + where + ": " + detail);
    if (!reported) {
      reported = true;
      show(detail + where);
    }
  });

  log(
    "info",
    "early script running; engine " +
      (navigator.userAgent || "?") +
      "; bridge " +
      (window.__TAURI_INTERNALS__ ? "present" : "MISSING")
  );

  // If the bundle has not announced itself shortly after load, the module
  // failed without an error event we could see (some engines report a
  // parse failure only to the console). Say so rather than stay blank.
  window.addEventListener("load", function () {
    setTimeout(function () {
      if (window.__rootmodeDiagnostics || reported) return;
      var root = document.getElementById("root");
      if (root && root.childElementCount > 0) return;
      reported = true;
      log("error", "the app bundle never started (no error event was delivered)");
      show("The app's code did not start, and the engine did not say why.");
    }, 3000);
  });
})();
