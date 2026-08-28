import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { describeError, diag, installDiagnostics, showFatal } from "./lib/diag";
import "./styles.css";

installDiagnostics();

try {
  ReactDOM.createRoot(document.getElementById("root")!).render(
    <React.StrictMode>
      <ErrorBoundary>
        <App />
      </ErrorBoundary>
    </React.StrictMode>,
  );
  diag("info", "react render requested");
} catch (e) {
  diag("error", `react could not mount: ${describeError(e)}`);
  showFatal("React could not mount.", describeError(e));
}
