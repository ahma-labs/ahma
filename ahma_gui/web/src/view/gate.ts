// The pinned permission question. The server decides what is asked and which
// answers are offered, in what order, and which are allowed; this draws exactly
// that and sends back the one the human chose.
//
// What binds this file (the human is being asked to widen a sandbox):
//  - No granting answer is ever the default. Nothing here calls focus(), so no
//    granting button is focused until the human moves to it; one answers only a
//    trusted activation (a click, or Space/Enter on the button they chose), never
//    a script's, and not in the moment after the question (re)draws. Keyboard and
//    assistive-technology users can grant as well as deny.
//  - Deny answers are always enabled, visually primary, and Enter/Escape inside
//    the panel (not on a granting button) choose the first of them.
//  - A scope/web question tells the server whether it is being *read*: visible,
//    focused document. Hidden is reported the moment that stops being true.
//  - The question stays until the server's view no longer has it.

import { postReport } from "../api.ts";
import { send } from "../act.ts";
import { el, setHidden, setText } from "../dom.ts";
import type { Mounted } from "../dom.ts";
import type { Gate, GateOption, Gates, WireIntent, WireReport, WireToolAnswer, WireWebAnswer } from "../wire.ts";

/** A click this soon after the question (re)drew is not a considered one. */
const SETTLE_MS = 500;
const BASE_TITLE = "ahma";
const TITLE_PREFIX = "● Question — ";

interface OptionEls {
  option: GateOption;
  button: HTMLButtonElement;
  hint: HTMLElement;
}

function idOf(gate: Gate): string {
  return gate.kind === "tool" ? gate.op_id : gate.decision_id;
}

/** Everything that decides how the panel is built; allowed/hint are patched in place. */
function structureOf(gate: Gate): string {
  return JSON.stringify({
    ...gate,
    options: gate.options.map((o) => ({ value: o.value, label: o.label, granting: o.granting })),
  });
}

function intentFor(gate: Gate, option: GateOption): WireIntent {
  switch (gate.kind) {
    case "scope":
      return { type: "answer_scope_grant", decision_id: gate.decision_id, option: option.value };
    case "web":
      return { type: "answer_web_approval", decision_id: gate.decision_id, answer: option.value as WireWebAnswer };
    case "tool":
      return { type: "answer_tool_approval", op_id: gate.op_id, answer: option.value as WireToolAnswer };
  }
}

function section(heading: string | null, body: string, mono = false): HTMLElement {
  return el(
    "div",
    { class: "gate-section" },
    heading !== null && el("div", { class: "gate-heading" }, heading),
    el("div", { class: mono ? "gate-body mono" : "gate-body" }, body),
  );
}

const advice = (text: string | null): HTMLElement | null =>
  text === null ? null : el("p", { class: "gate-advice quiet" }, text);

/** What the question says, apart from its title and its buttons. */
function bodyFor(g: Gate): HTMLElement[] {
  const parts = ((): Array<HTMLElement | null> => {
    switch (g.kind) {
      case "scope":
        return [...g.sections.map((s) => section(s.heading, s.body)), advice(g.advice)];
      case "web":
        return [section("URL", g.url, true), g.tool === null ? null : section("Requested by", g.tool, true)];
      case "tool":
        return [section(null, g.description), advice(g.note)];
    }
  })();
  return parts.filter((p): p is HTMLElement => p !== null);
}

function kickerFor(g: Gate): string {
  return { scope: "Sandbox access", web: "Network request", tool: "Tool approval" }[g.kind];
}

function titleFor(g: Gate): string {
  return g.kind === "scope" ? g.title : g.kind === "web" ? g.domain : g.tool;
}

function makeOption(option: GateOption, onClick: (event: MouseEvent, option: GateOption) => void): OptionEls {
  const button = el("button", {
    type: "button",
    class: option.granting ? "btn btn-grant" : "btn btn-primary",
    "data-granting": String(option.granting),
  }, option.label);
  button.addEventListener("click", (event) => onClick(event, option));
  return { option, button, hint: el("span", { class: "opt-hint quiet" }) };
}

/**
 * Read-before-grant reporting: tells the server whether the scope/web question
 * on screen is being *read* (visible, focused document), in order, so a late
 * "visible" can never overtake a "hidden".
 */
interface ReadReporter {
  /** The scope/web question now on screen, or null when there is none. */
  show(id: string | null): void;
  /** The stream (re)connected: say again what the server was last told. */
  resync(): void;
  destroy(): void;
}

function createReadReporter(): ReadReporter {
  let reportId: string | null = null; // the scope/web question on screen, if any
  let reported: string | null = null; // the one the server was last told is visible
  let queue: Promise<void> = Promise.resolve();

  function report(r: WireReport): void {
    queue = queue.then(() => postReport(r)).catch(() => {});
  }

  function evaluate(): void {
    const readable = reportId !== null && document.visibilityState === "visible" && document.hasFocus();
    if (reported !== null && (!readable || reported !== reportId)) {
      report({ type: "gate_hidden", decision_id: reported });
      reported = null;
    }
    if (readable && reported === null && reportId !== null) {
      reported = reportId;
      report({ type: "gate_visible", decision_id: reportId });
    }
  }

  const triggers: Array<[EventTarget, string]> = [
    [document, "visibilitychange"],
    [window, "focus"],
    [window, "blur"],
  ];
  for (const [target, type] of triggers) target.addEventListener(type, evaluate);
  const onPageHide = (): void => show(null);
  window.addEventListener("pagehide", onPageHide);

  function show(id: string | null): void {
    reportId = id;
    evaluate();
  }

  return {
    show,
    resync() {
      if (reported !== null) report({ type: "gate_visible", decision_id: reported });
      else evaluate();
    },
    destroy() {
      for (const [target, type] of triggers) target.removeEventListener(type, evaluate);
      window.removeEventListener("pagehide", onPageHide);
      show(null);
    },
  };
}

export interface GateView extends Mounted<Gates | null> {
  /** The stream (re)connected: the server may know nothing of what this tab shows, so say it again. */
  resync(): void;
}

export function mount(root: HTMLElement): GateView {
  const kicker = el("span", { class: "gate-kicker" });
  const count = el("span", { class: "gate-count quiet" });
  const content = el("div", { class: "gate-content" });
  const panel = el(
    "section",
    { class: "gate", role: "alertdialog", tabindex: 0, "aria-labelledby": "gate-title" },
    el("div", { class: "gate-head" }, kicker, count),
    content,
  );
  panel.hidden = true;
  root.appendChild(panel);

  const reporter = createReadReporter();
  let gate: Gate | null = null;
  let structure = "";
  let options: OptionEls[] = [];
  let shownAt = 0;
  let answering = false;

  // ── answering ──

  function deny(): void {
    const option = gate?.options.find((o) => !o.granting);
    if (gate && option) void answer(gate, option);
  }

  async function answer(g: Gate, option: GateOption): Promise<void> {
    if (answering) return; // one request at a time; the server's view decides what happens next
    answering = true;
    try {
      await send(intentFor(g, option));
    } finally {
      answering = false;
    }
  }

  /** A script cannot make a trusted event; a human at the keyboard or a pointer can. */
  function isConsidered(event: MouseEvent): boolean {
    return event.isTrusted && performance.now() - shownAt >= SETTLE_MS;
  }

  function onClick(event: MouseEvent, g: Gate, option: GateOption): void {
    if (option.granting && !isConsidered(event)) return;
    void answer(g, option);
  }

  // Escape denies, and so does Enter when focus is in the panel but on no
  // button. Enter on a button activates that button, as a keyboard user expects.
  panel.addEventListener("keydown", (event) => {
    const onButton = event.target instanceof Element && event.target.closest("button") !== null;
    if (event.key === "Escape" || (event.key === "Enter" && !onButton)) {
      event.preventDefault();
      if (!event.repeat) deny();
    }
  });

  // ── drawing ──

  function build(g: Gate): void {
    shownAt = performance.now();
    options = g.options.map((option) => makeOption(option, (event, o) => onClick(event, g, o)));
    setText(kicker, kickerFor(g));
    content.replaceChildren(
      el("h2", { id: "gate-title", class: g.kind === "scope" ? "gate-title" : "gate-title mono" }, titleFor(g)),
      ...bodyFor(g),
      el("div", { class: "gate-options" }, ...options.map((o) => el("div", { class: "opt" }, o.button, o.hint))),
    );
  }

  function patchOptions(g: Gate): void {
    g.options.forEach((option, i) => {
      const o = options[i];
      if (!o) return;
      o.option = option;
      const disabled = option.granting && !option.allowed; // a deny is never disabled
      if (o.button.disabled !== disabled) o.button.disabled = disabled;
      setText(o.hint, option.hint ?? "");
      setHidden(o.hint, option.hint === null);
    });
  }

  function clear(): void {
    setHidden(panel, true);
    structure = "";
    options = [];
    content.replaceChildren();
    document.title = BASE_TITLE;
    reporter.show(null);
  }

  return {
    update(gates) {
      const top = gates?.top ?? null;
      const previous = gate;
      gate = top;
      if (top === null) return clear();
      const next = structureOf(top);
      if (previous === null || next !== structure) {
        structure = next;
        build(top);
      }
      patchOptions(top);
      const waiting = gates?.waiting ?? 1;
      setText(count, waiting > 1 ? `1 of ${waiting} waiting` : "");
      setHidden(panel, false);
      document.title = TITLE_PREFIX + BASE_TITLE;
      reporter.show(top.kind === "tool" ? null : idOf(top));
    },
    resync: reporter.resync,
    destroy() {
      reporter.destroy();
      document.title = BASE_TITLE;
      panel.remove();
    },
  };
}
