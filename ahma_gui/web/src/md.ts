// A reply arrives already parsed into blocks and spans; this builds nodes from
// that tree. Every string goes in as text. Links are inert: the text and the
// full destination are shown, and nothing is ever an <a>.

import { el } from "./dom.ts";
import type { Block, Span } from "./wire.ts";

/** Deeper nesting than a person writes is not rendered. */
const MAX_DEPTH = 16;

export function renderSpans(spans: readonly Span[]): Node[] {
  return spans.map((span): Node => {
    switch (span.t) {
      case "text": {
        let node: Node = document.createTextNode(span.s);
        if (span.strong === true) node = el("strong", {}, node);
        if (span.em === true) node = el("em", {}, node);
        return node;
      }
      case "code":
        return el("code", { class: "md-code" }, span.s);
      case "link":
        return el(
          "span",
          { class: "md-link" },
          el("span", { class: "md-link-text" }, span.s),
          el("span", { class: "md-href" }, ` (${span.href})`),
        );
      default:
        return document.createTextNode("");
    }
  });
}

export function renderBlocks(blocks: readonly Block[], depth = 0): HTMLElement[] {
  if (depth > MAX_DEPTH) return [el("div", { class: "md-unknown" }, "…")];
  return blocks.map((block): HTMLElement => {
    switch (block.t) {
      case "para":
        return el("p", { class: "md-p" }, ...renderSpans(block.spans));
      case "heading": {
        const level = Math.min(6, Math.max(1, Math.trunc(block.level) || 1));
        return el(
          "div",
          { class: `md-h md-h${level}`, role: "heading", "aria-level": level },
          ...renderSpans(block.spans),
        );
      }
      case "code": {
        const pre = el("pre", { class: "md-pre" }, el("code", {}, block.text));
        if (block.lang === null || block.lang === "") return pre;
        return el("div", { class: "md-fence" }, el("div", { class: "md-lang" }, block.lang), pre);
      }
      case "list": {
        const list = el(block.ordered ? "ol" : "ul", { class: "md-list" });
        if (block.ordered && block.start !== null) (list as HTMLOListElement).start = block.start;
        for (const item of block.items) {
          list.appendChild(el("li", {}, ...renderBlocks(item, depth + 1)));
        }
        return list;
      }
      case "quote":
        return el("blockquote", { class: "md-quote" }, ...renderBlocks(block.blocks, depth + 1));
      case "rule":
        return el("hr", { class: "md-rule" });
      default:
        return el("div", { class: "md-unknown" }, "(content this page cannot show)");
    }
  });
}
