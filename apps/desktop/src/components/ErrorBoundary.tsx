import { Component, type ErrorInfo, type ReactNode } from "react";
import { describeError, diag } from "../lib/diag";

interface State {
  error: string | null;
}

/**
 * A render error anywhere below unmounts the whole tree — React's rule, and
 * the reason a single bad component reads as "the app is blank". This
 * catches it, writes it to the log, and shows it, so a blank window is
 * never the only evidence.
 */
export class ErrorBoundary extends Component<{ children: ReactNode }, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: unknown): State {
    return { error: describeError(error) };
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    diag("error", `react render failed: ${describeError(error)}\ncomponent stack:${info.componentStack ?? ""}`);
  }

  render() {
    if (this.state.error === null) return this.props.children;
    return (
      <pre
        style={{
          margin: 0,
          padding: 28,
          font: "13px/1.5 ui-monospace, Menlo, monospace",
          whiteSpace: "pre-wrap",
          wordBreak: "break-word",
          minHeight: "100vh",
          boxSizing: "border-box",
        }}
      >
        {"rootmode hit an error while drawing the page.\n\n"}
        {this.state.error}
        {"\n\nThe log file next to the app's data has the rest."}
      </pre>
    );
  }
}
