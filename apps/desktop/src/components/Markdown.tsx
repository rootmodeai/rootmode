import type { Components } from "react-markdown";
import Markdown from "react-markdown";
import remarkGfmLite from "../lib/gfm";
import { openUrl } from "@tauri-apps/plugin-opener";

function open(href: string) {
  if (!/^https?:\/\//i.test(href)) return;
  void openUrl(href);
}

const components: Components = {
  a({ href, children }) {
    const url = href ?? "";
    return (
      <a
        href={url}
        onClick={(e) => {
          e.preventDefault();
          open(url);
        }}
      >
        {children}
      </a>
    );
  },
  // Remote pictures would be fetched by this process. A data URL is already
  // in the message; anything else is shown as its alt text, not loaded.
  img({ src, alt }) {
    if (src?.startsWith("data:")) {
      return <img src={src} alt={alt ?? ""} />;
    }
    return <span className="md-img">{alt || src || "image"}</span>;
  },
};

export function MarkdownBody({ text }: { text: string }) {
  return (
    <div className="bubble md">
      <Markdown remarkPlugins={[remarkGfmLite]} components={components}>
        {text}
      </Markdown>
    </div>
  );
}
