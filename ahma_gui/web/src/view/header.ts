// The top bar: where this is, what confines it, what is connected, how busy it is.

import { el, setHidden, setText } from "../dom.ts";
import type { Mounted } from "../dom.ts";
import { shortenPath } from "../format.ts";
import type { StreamState } from "../stream.ts";
import type { Header } from "../wire.ts";

export interface HeaderData {
  header: Header | null;
  link: StreamState;
}

const LINK_TEXT: Record<StreamState, string> = {
  connecting: "connecting",
  live: "live",
  reconnecting: "reconnecting",
  closed: "server stopped",
  unauthorized: "signed out",
};

export function mount(root: HTMLElement): Mounted<HeaderData> {
  const workspace = el("button", { type: "button", class: "ws mono", "aria-expanded": "false" });
  const badge = el("span", { class: "badge" });
  const hub = dot("hub");
  const server = dot("server");
  const link = dot("");
  const model = el("span", { class: "model quiet" });
  const counts = el("span", { class: "counts" });

  const disclosure = el("div", { class: "disclosure", role: "note" });
  const detail = el("div", { class: "hdr-detail" });
  detail.hidden = true;
  disclosure.hidden = true;

  workspace.addEventListener("click", () => {
    detail.hidden = !detail.hidden;
    workspace.setAttribute("aria-expanded", String(!detail.hidden));
  });

  const bar = el(
    "div",
    { class: "hdr-bar" },
    el("span", { class: "brand" }, "ahma"),
    workspace,
    badge,
    el("span", { class: "hdr-spacer" }),
    el("span", { class: "dots" }, hub.node, server.node, link.node),
    model,
    counts,
  );
  root.append(el("header", { class: "hdr" }, bar, disclosure, detail));

  function patchDetail(h: Header): void {
    const rows: Array<[string, string[]]> = [
      ["Workspace", [h.workspace]],
      ["Transport", [h.transport]],
      ["Writable", h.sandbox.write],
      ["Readable", h.sandbox.read],
    ];
    if (h.sandbox.note !== null) rows.push(["Note", [h.sandbox.note]]);
    detail.replaceChildren(
      ...rows.map(([name, values]) =>
        el(
          "div",
          { class: "kv" },
          el("div", { class: "k" }, name),
          el("div", { class: "v mono" }, ...(values.length === 0 ? ["(none)"] : values.map((v) => el("div", { class: "path" }, v)))),
        ),
      ),
    );
  }

  let lastDetail: Header | null = null;

  return {
    update({ header, link: linkState }) {
      link.set(linkState === "live", LINK_TEXT[linkState]);
      if (header === null) {
        setText(workspace, "waiting for the session…");
        setText(badge, "");
        setHidden(badge, true);
        return;
      }
      setText(workspace, shortenPath(header.workspace));
      const calm = header.sandbox.enforced && header.sandbox.sole_authority;
      setText(badge, header.sandbox.label);
      setHidden(badge, false);
      badge.dataset["tone"] = calm ? "ok" : "warn";
      hub.set(header.hub, header.hub ? "hub up" : "hub down");
      server.set(header.server, header.server ? "server up" : "server down");
      setText(model, header.model ?? "");
      setText(counts, `${header.counts.running} running · ${header.counts.queued} queued · ${header.counts.failed} failed`);
      counts.dataset["failed"] = header.counts.failed > 0 ? "true" : "false";
      const text = header.sandbox.disclosure;
      setText(disclosure, text ?? "");
      setHidden(disclosure, text === null);
      if (lastDetail !== header) {
        lastDetail = header;
        patchDetail(header);
      }
    },
  };
}

/** A small status dot with a text label that screen readers (and hover) can use. */
function dot(name: string): { node: HTMLElement; set(up: boolean, label: string): void } {
  const mark = el("span", { class: "dot", "aria-hidden": "true" });
  const label = el("span", { class: "dot-label" }, name);
  const node = el("span", { class: "dot-wrap" }, mark, label);
  return {
    node,
    set(up, text) {
      mark.dataset["up"] = up ? "true" : "false";
      setText(label, text);
    },
  };
}
