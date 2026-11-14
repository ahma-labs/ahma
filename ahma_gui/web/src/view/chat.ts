// The conversation: a transcript, a quiet status line, and a box to type in.
// Slash commands are just text; the server answers them (or says it cannot here).

import { send } from "../act.ts";
import { el, reconcile, setHidden, setText } from "../dom.ts";
import type { Keyed, Mounted } from "../dom.ts";
import { renderBlocks } from "../md.ts";
import type { Chat, Entry } from "../wire.ts";

const BOTTOM_SLACK_PX = 24;

const entryKey = (e: Entry): string => `${e.kind}:${e.id}`;

function details(cls: string, summary: HTMLElement, ...body: HTMLElement[]): HTMLDetailsElement {
  return el("details", { class: cls }, el("summary", {}, summary), ...body);
}

/** Entries are patched in place; one whose content did not change is left alone. */
function createEntry(first: Entry): Keyed<Entry> {
  let sig = "";
  switch (first.kind) {
    case "user": {
      const body = el("div", { class: "msg-text" });
      const node = el("div", { class: "msg msg-user" }, body);
      return {
        node,
        patch(e) {
          if (e.kind === "user") setText(body, e.text);
        },
      };
    }
    case "assistant": {
      const body = el("div", { class: "md" });
      const node = el("div", { class: "msg msg-assistant" }, body);
      return {
        node,
        patch(e) {
          if (e.kind !== "assistant") return;
          const next = JSON.stringify(e.blocks);
          if (next !== sig) {
            sig = next;
            body.replaceChildren(...renderBlocks(e.blocks));
          }
          node.dataset["streaming"] = String(e.streaming);
        },
      };
    }
    case "thinking": {
      const label = el("span", {});
      const text = el("pre", { class: "thinking-text" });
      const node = details("msg msg-thinking", label, text);
      return {
        node,
        patch(e) {
          if (e.kind !== "thinking") return;
          setText(label, e.streaming ? "Thinking…" : "Thought");
          setText(text, e.text);
        },
      };
    }
    case "tool": {
      const name = el("span", { class: "tool-name mono" });
      const title = el("span", { class: "tool-title" });
      const state = el("span", { class: "tag" });
      const args = el("pre", { class: "tool-pre mono" });
      const result = el("pre", { class: "tool-pre mono" });
      const resultLabel = el("div", { class: "tool-label quiet" }, "Result");
      const node = details(
        "msg msg-tool",
        el("span", { class: "tool-sum" }, name, title, state),
        el("div", { class: "tool-label quiet" }, "Arguments"),
        args,
        resultLabel,
        result,
      );
      return {
        node,
        patch(e) {
          if (e.kind !== "tool") return;
          setText(name, e.name);
          setText(title, e.title);
          setText(state, e.failed ? "failed" : e.result === null ? "running" : "");
          state.className = e.failed ? "tag tag-danger" : "tag";
          setHidden(state, !e.failed && e.result !== null);
          setText(args, e.args);
          setHidden(resultLabel, e.result === null);
          setHidden(result, e.result === null);
          setText(result, e.result ?? "");
          node.dataset["failed"] = String(e.failed);
        },
      };
    }
    case "notice": {
      const body = el("div", {});
      const node = el("div", { class: "msg msg-notice", role: "note" }, body);
      return {
        node,
        patch(e) {
          if (e.kind === "notice") setText(body, e.text);
        },
      };
    }
  }
}

export function mount(root: HTMLElement): Mounted<Chat | null> {
  const target = el("span", { class: "chat-target quiet mono" });
  const transcript = el("div", { class: "transcript", role: "log", "aria-live": "off", tabindex: 0, "aria-label": "Conversation" });
  const empty = el("p", { class: "empty" }, "Nothing said yet. Ask something below, or type / for commands.");
  const status = el("div", { class: "chat-status quiet", role: "status" });
  const stop = el("button", { type: "button", class: "btn btn-small" }, "Stop");
  const input = el("textarea", { class: "input chat-input", rows: 2, placeholder: "Message — Enter sends, Shift+Enter for a new line", "aria-label": "Message" });
  const sendButton = el("button", { type: "button", class: "btn btn-primary" }, "Send");
  const entries = new Map<string, Keyed<Entry>>();
  let stick = true;
  let sending = false;

  transcript.addEventListener("scroll", () => {
    stick = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < BOTTOM_SLACK_PX;
  });
  stop.addEventListener("click", () => void send({ type: "cancel_turn" }));

  async function submit(): Promise<void> {
    const text = input.value;
    if (text.trim() === "" || sending) return; // never an empty message
    sending = true;
    try {
      if (await send({ type: "submit", text })) {
        input.value = "";
        stick = true;
      }
    } finally {
      sending = false;
    }
  }
  sendButton.addEventListener("click", () => void submit());
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void submit();
    }
  });

  root.appendChild(
    el(
      "section",
      { class: "panel chat", "aria-label": "Chat" },
      el("div", { class: "panel-head" }, el("h2", { class: "panel-title" }, "Chat"), target),
      el("div", { class: "transcript-wrap" }, transcript, empty),
      el("div", { class: "chat-bar" }, status, stop),
      el("div", { class: "composer" }, input, sendButton),
    ),
  );
  const list = el("div", { class: "entries" });
  transcript.appendChild(list);

  return {
    update(chat) {
      if (chat === null) return;
      const keepBottom = stick;
      setHidden(empty, chat.entries.length > 0);
      reconcile(list, entries, chat.entries, entryKey, createEntry);
      setText(target, chat.target === null ? "" : `→ ${chat.target}`);
      const turn = chat.turn;
      setText(status, turn === null ? "" : `${turn.phase.replaceAll("-", " ")}${turn.tool === null ? "" : ` · ${turn.tool}`}`);
      setHidden(stop, turn === null);
      if (keepBottom) transcript.scrollTop = transcript.scrollHeight;
    },
  };
}
