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
//  - A scope, web or link question tells the server whether it is being
//    *read*: visible, focused document. Hidden is reported the moment that stops being true.
//  - The question stays until the server's view no longer has it.

import { send } from "../act.ts";
import { intentFor } from "../answer.ts";
import { postReport } from "../api.ts";
import { el, setHidden, setText } from "../dom.ts";
import type { Mounted } from "../dom.ts";
import type { Gate, GateOption, Gates, WireReport } from "../wire.ts";

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

/** The body parts each kind of question has, a null for one it leaves out. */
function partsFor(g: Gate): Array<HTMLElement | null> {
  switch (g.kind) {
    case "scope":
      return [...g.sections.map((s) => section(s.heading, s.body)), advice(g.advice)];
    case "web":
      return [section("URL", g.url, true), g.tool === null ? null : section("Requested by", g.tool, true)];
    case "link":
      return [section("Refused in", g.workspace, true), section(null, g.text), advice(g.note)];
    case "tool":
      return [section(null, g.description), advice(g.note)];
  }
}

/** What the question says, apart from its title and its buttons. */
function bodyFor(g: Gate): HTMLElement[] {
  return partsFor(g).filter((p): p is HTMLElement => p !== null);
}

function kickerFor(g: Gate): string {
  return { scope: "Sandbox access", web: "Network request", link: "Open a web link", tool: "Tool approval" }[g.kind];
}

function titleFor(g: Gate): string {
  switch (g.kind) {
    case "scope":
      return g.title;
    case "web":
      return g.domain;
    case "link":
      return g.site;
    case "tool":
      return g.tool;
  }
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

/** Bring one drawn option up to date with the server's: whether it is allowed, and its hint. */
function patchOption(o: OptionEls, option: GateOption): void {
  o.option = option;
  const disabled = option.granting && !option.allowed; // a deny is never disabled
  if (o.button.disabled !== disabled) o.button.disabled = disabled;
  setText(o.hint, option.hint ?? "");
  setHidden(o.hint, option.hint === null);
}

function patchOptions(options: readonly OptionEls[], g: Gate): void {
  g.options.forEach((option, i) => {
    const o = options[i];
    if (o) patchOption(o, option);
  });
}

// ── read-before-grant reporting ──

/**
 * Read-before-grant reporting: tells the server whether the scope, web or link
 * question on screen is being *read* (visible, focused document), in order, so a late
 * "visible" can never overtake a "hidden".
 */
interface ReadReporter {
  /** The scope, web or link question now on screen, or null when there is none. */
  show(id: string | null): void;
  /** The stream (re)connected: say again what the server was last told. */
  resync(): void;
  destroy(): void;
}

interface ReporterState {
  /** The scope, web or link question on screen, if any. */
  reportId: string | null;
  /** The one the server was last told is visible. */
  reported: string | null;
  /** Reports go out one after another, never overtaking. */
  queue: Promise<void>;
}

function report(s: ReporterState, r: WireReport): void {
  s.queue = s.queue.then(() => postReport(r)).catch(() => {});
}

/** The question on screen when the human can be reading it (visible, focused document), else null. */
function readId(s: ReporterState): string | null {
  if (s.reportId === null) return null;
  return document.visibilityState === "visible" && document.hasFocus() ? s.reportId : null;
}

/** Tell the server what changed: hidden for one no longer read, then visible for the one now read. */
function evaluate(s: ReporterState): void {
  const read = readId(s);
  if (s.reported !== null && s.reported !== read) {
    report(s, { type: "gate_hidden", decision_id: s.reported });
    s.reported = null;
  }
  if (read !== null && s.reported === null) {
    s.reported = read;
    report(s, { type: "gate_visible", decision_id: read });
  }
}

function createReadReporter(): ReadReporter {
  const s: ReporterState = { reportId: null, reported: null, queue: Promise.resolve() };
  const onChange = (): void => evaluate(s);
  const show = (id: string | null): void => {
    s.reportId = id;
    evaluate(s);
  };
  const onPageHide = (): void => show(null);

  const triggers: Array<[EventTarget, string]> = [
    [document, "visibilitychange"],
    [window, "focus"],
    [window, "blur"],
  ];
  for (const [target, type] of triggers) target.addEventListener(type, onChange);
  window.addEventListener("pagehide", onPageHide);

  return {
    show,
    resync() {
      if (s.reported !== null) report(s, { type: "gate_visible", decision_id: s.reported });
      else evaluate(s);
    },
    destroy() {
      for (const [target, type] of triggers) target.removeEventListener(type, onChange);
      window.removeEventListener("pagehide", onPageHide);
      show(null);
    },
  };
}

// ── the panel ──

export interface GateView extends Mounted<Gates | null> {
  /** The stream (re)connected: the server may know nothing of what this tab shows, so say it again. */
  resync(): void;
}

interface Panel {
  readonly panel: HTMLElement;
  readonly kicker: HTMLElement;
  readonly count: HTMLElement;
  readonly content: HTMLElement;
  readonly reporter: ReadReporter;
  /** The question on screen, or null when there is none. */
  gate: Gate | null;
  /** `structureOf` the question as last built; "" when nothing is built. */
  structure: string;
  options: OptionEls[];
  /** When the question last (re)drew, for SETTLE_MS. */
  shownAt: number;
  /** An answer is on its way to the server. */
  answering: boolean;
}

function createPanel(root: HTMLElement): Panel {
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
  return { panel, kicker, count, content, reporter, gate: null, structure: "", options: [], shownAt: 0, answering: false };
}

// ── answering ──

async function answer(p: Panel, g: Gate, option: GateOption): Promise<void> {
  if (p.answering) return; // one request at a time; the server's view decides what happens next
  const intent = intentFor(g, option);
  if (intent === null) return; // not this surface's to send; the button says where it is
  p.answering = true;
  try {
    await send(intent);
  } finally {
    p.answering = false;
  }
}

function deny(p: Panel): void {
  const g = p.gate;
  const option = g?.options.find((o) => !o.granting);
  if (g && option) void answer(p, g, option);
}

/** A script cannot make a trusted event; a human at the keyboard or a pointer can. */
function isConsidered(p: Panel, event: MouseEvent): boolean {
  return event.isTrusted && performance.now() - p.shownAt >= SETTLE_MS;
}

function onOptionClick(p: Panel, event: MouseEvent, g: Gate, option: GateOption): void {
  if (option.granting && !isConsidered(p, event)) return;
  void answer(p, g, option);
}

/** Escape denies, and so does Enter when focus is in the panel but on no button. */
function isDenyKey(event: KeyboardEvent): boolean {
  if (event.key === "Escape") return true;
  if (event.key !== "Enter") return false;
  // Enter on a button activates that button, as a keyboard user expects.
  return !(event.target instanceof Element && event.target.closest("button") !== null);
}

function onKeydown(p: Panel, event: KeyboardEvent): void {
  if (!isDenyKey(event)) return;
  event.preventDefault();
  if (!event.repeat) deny(p);
}

// ── drawing ──

function build(p: Panel, g: Gate): void {
  p.shownAt = performance.now();
  p.options = g.options.map((option) => makeOption(option, (event, o) => onOptionClick(p, event, g, o)));
  setText(p.kicker, kickerFor(g));
  p.content.replaceChildren(
    el("h2", { id: "gate-title", class: g.kind === "scope" ? "gate-title" : "gate-title mono" }, titleFor(g)),
    ...bodyFor(g),
    el("div", { class: "gate-options" }, ...p.options.map((o) => el("div", { class: "opt" }, o.button, o.hint))),
  );
}

function clear(p: Panel): void {
  setHidden(p.panel, true);
  p.structure = "";
  p.options = [];
  p.content.replaceChildren();
  document.title = BASE_TITLE;
  p.reporter.show(null);
}

/** Draw `top`, rebuilding only when it is a new question or its structure changed. */
function present(p: Panel, top: Gate, previous: Gate | null, waiting: number): void {
  const next = structureOf(top);
  if (previous === null || next !== p.structure) {
    p.structure = next;
    build(p, top);
  }
  patchOptions(p.options, top);
  setText(p.count, waiting > 1 ? `1 of ${waiting} waiting` : "");
  setHidden(p.panel, false);
  document.title = TITLE_PREFIX + BASE_TITLE;
  p.reporter.show(top.kind === "tool" ? null : idOf(top));
}

function updatePanel(p: Panel, gates: Gates | null): void {
  const top = gates?.top ?? null;
  const previous = p.gate;
  p.gate = top;
  if (top === null) return clear(p);
  present(p, top, previous, gates?.waiting ?? 1);
}

export function mount(root: HTMLElement): GateView {
  const p = createPanel(root);
  p.panel.addEventListener("keydown", (event) => onKeydown(p, event));
  return {
    update: (gates) => updatePanel(p, gates),
    resync: p.reporter.resync,
    destroy() {
      p.reporter.destroy();
      document.title = BASE_TITLE;
      p.panel.remove();
    },
  };
}
