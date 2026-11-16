// The pairing screen: the human pastes the short single-use code `ahma web`
// copied to their clipboard, or types it from their terminal. The server
// decides whether it is right; this shows what it said. The page never reads
// the clipboard itself: the human pastes.

import { ApiError } from "../api.ts";
import { el, setText } from "../dom.ts";
import { looksLikeFullCode } from "../format.ts";
import type { Mounted } from "../dom.ts";

export interface PairDeps {
  /** Resolves when paired; rejects with an ApiError carrying the server's message. */
  submit(code: string): Promise<void>;
}

export function mount(root: HTMLElement, deps: PairDeps): Mounted<{ notice: string | null }> {
  const input = el("input", {
    type: "text",
    id: "pair-code",
    class: "input mono pair-code",
    placeholder: "ABCD-EFGH",
    "aria-describedby": "pair-message",
  });
  input.autocomplete = "off";
  input.spellcheck = false;
  input.autocapitalize = "characters";
  input.maxLength = 32;

  const button = el("button", { type: "submit", class: "btn btn-primary" }, "Pair");
  const message = el("p", { id: "pair-message", class: "pair-message", role: "alert" });
  const form = el(
    "form",
    { class: "pair-form" },
    el("label", { for: "pair-code", class: "pair-label" }, "Pairing code"),
    el("div", { class: "pair-row" }, input, button),
    message,
  );

  let busy = false;
  const submit = () => {
    const code = input.value.trim();
    if (busy || code === "") return;
    busy = true;
    button.disabled = true;
    setText(message, "");
    deps
      .submit(code)
      .catch((err: unknown) => {
        setText(message, err instanceof ApiError ? err.message : "Could not pair.");
        input.select();
      })
      .finally(() => {
        busy = false;
        button.disabled = false;
      });
  };
  form.addEventListener("submit", (event) => {
    event.preventDefault(); // never a navigation; the code goes to /api/pair by fetch
    submit();
  });
  // A pasted code is whole: send it without waiting for a click.
  input.addEventListener("paste", () => {
    queueMicrotask(() => {
      if (looksLikeFullCode(input.value)) submit();
    });
  });

  const card = el(
    "main",
    { class: "pair" },
    el("div", { class: "pair-card" }, el("h1", { class: "pair-title" }, "ahma"), el(
        "p",
        { class: "pair-help" },
        "Paste the pairing code ahma web copied to your clipboard (⌘V or Ctrl+V), or type the one shown in your terminal, to watch and answer from this browser.",
      ), form),
  );
  root.appendChild(card);
  input.focus();

  return {
    update: ({ notice }) => {
      if (notice !== null) setText(message, notice);
    },
    destroy: () => {
      input.value = "";
      card.remove();
    },
  };
}
