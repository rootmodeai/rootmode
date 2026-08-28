// GitHub-flavoured markdown — tables, strikethrough, task lists, footnotes
// and bare links — without `remark-gfm`.
//
// `remark-gfm` pulls in `mdast-util-gfm-autolink-literal`, whose fallback
// pass carries a regex lookbehind. A regex literal is checked when the
// script is *parsed*, so on any WebKit older than Safari 16.4 (macOS 12
// and 13 before their last point releases) the whole bundle is a syntax
// error and the window stays blank. Nothing in it ever ran; nothing could
// report it.
//
// The tokenizer side of GFM (`micromark-extension-gfm`) autolinks URLs and
// emails during parsing with no regex at all. So this uses that tokenizer,
// and supplies the tree-building handlers for autolinks itself — the same
// dozen lines the upstream module has, minus the regex pass that only
// exists for callers who skip the tokenizer.
//
// `scripts/check-bundle.mjs` fails the build if a lookbehind ever comes
// back through another dependency.

import type { CompileContext, Extension as FromMarkdownExtension, Token } from "mdast-util-from-markdown";
import type { Root } from "mdast";
import { gfmFootnoteFromMarkdown } from "mdast-util-gfm-footnote";
import { gfmStrikethroughFromMarkdown } from "mdast-util-gfm-strikethrough";
import { gfmTableFromMarkdown } from "mdast-util-gfm-table";
import { gfmTaskListItemFromMarkdown } from "mdast-util-gfm-task-list-item";
import { gfm } from "micromark-extension-gfm";
// Type-only: registers `micromarkExtensions` / `fromMarkdownExtensions` on
// unified's `Data`, the way remark-parse declares them.
import type {} from "remark-parse";
import type { Plugin } from "unified";

/** Tree handlers for the tokens `micromark-extension-gfm-autolink-literal` emits. */
function autolinkLiteralFromMarkdown(): FromMarkdownExtension {
  return {
    enter: {
      literalAutolink: enterLiteralAutolink,
      literalAutolinkEmail: enterLiteralAutolinkValue,
      literalAutolinkHttp: enterLiteralAutolinkValue,
      literalAutolinkWww: enterLiteralAutolinkValue,
    },
    exit: {
      literalAutolink: exitLiteralAutolink,
      literalAutolinkEmail: exitLiteralAutolinkEmail,
      literalAutolinkHttp: exitLiteralAutolinkHttp,
      literalAutolinkWww: exitLiteralAutolinkWww,
    },
  };
}

function enterLiteralAutolink(this: CompileContext, token: Token) {
  this.enter({ type: "link", title: null, url: "", children: [] }, token);
}

function enterLiteralAutolinkValue(this: CompileContext, token: Token) {
  this.config.enter.autolinkProtocol.call(this, token);
}

function exitLiteralAutolinkHttp(this: CompileContext, token: Token) {
  this.config.exit.autolinkProtocol.call(this, token);
}

function exitLiteralAutolinkWww(this: CompileContext, token: Token) {
  this.config.exit.data.call(this, token);
  const node = this.stack[this.stack.length - 1];
  if (node.type === "link") {
    node.url = "http://" + this.sliceSerialize(token);
  }
}

function exitLiteralAutolinkEmail(this: CompileContext, token: Token) {
  this.config.exit.autolinkEmail.call(this, token);
}

function exitLiteralAutolink(this: CompileContext, token: Token) {
  this.exit(token);
}

/** A drop-in for `remarkGfm` in `remarkPlugins`. */
const remarkGfmLite: Plugin<[], Root> = function () {
  const data = this.data();
  const micromarkExtensions = data.micromarkExtensions || (data.micromarkExtensions = []);
  const fromMarkdownExtensions = data.fromMarkdownExtensions || (data.fromMarkdownExtensions = []);

  micromarkExtensions.push(gfm());
  fromMarkdownExtensions.push([
    autolinkLiteralFromMarkdown(),
    gfmFootnoteFromMarkdown(),
    gfmStrikethroughFromMarkdown(),
    gfmTableFromMarkdown(),
    gfmTaskListItemFromMarkdown(),
  ]);
};

export default remarkGfmLite;
