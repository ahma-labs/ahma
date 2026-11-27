// The server's log tail: collapsible, level-coloured, filtered by text here
// (a display filter only; the server's lines are never changed).

import { el, setHidden, setText } from "../dom.ts";
import type { Mounted } from "../dom.ts";
import type { Log, LogLine } from "../wire.ts";

const BOTTOM_SLACK_PX = 24;

interface LineEls {
  node: HTMLElement;
  set(line: LogLine): void;
}

function createLine(): LineEls {
  const ts = el("span", { class: "log-ts quiet mono" });
  const level = el("span", { class: "log-level mono" });
  const text = el("span", { class: "log-text mono" });
  const node = el("div", { class: "log-line" }, ts, level, text);
  return {
    node,
    set(line) {
      node.dataset["level"] = line.level;
      setText(ts, line.ts);
      setText(level, line.level);
      setText(text, line.text);
    },
  };
}

interface Panel {
  readonly toggle: HTMLElement;
  readonly summary: HTMLElement;
  readonly filter: HTMLInputElement;
  readonly active: HTMLElement;
  readonly lines: HTMLElement;
  readonly body: HTMLElement;
  /** One reusable row per line on screen, in order. */
  readonly pool: LineEls[];
  open: boolean;
  log: Log | null;
  /** Keep the view pinned to the newest line (the human has not scrolled up). */
  stick: boolean;
  /** The lines changed while the panel was closed. */
  needsDraw: boolean;
}

function createPanel(root: HTMLElement): Panel {
  const summary = el("span", { class: "log-summary quiet" });
  const toggle = el("button", { type: "button", class: "log-toggle", "aria-expanded": "false" }, el("span", { class: "caret", "aria-hidden": "true" }, "▸"), el("span", { class: "panel-title" }, "Log"), summary);
  const filter = el("input", { type: "text", class: "input log-filter", placeholder: "Filter", "aria-label": "Filter log lines" });
  const active = el("span", { class: "quiet mono log-active" });
  const lines = el("div", { class: "log-lines", role: "log", "aria-live": "off", tabindex: 0, "aria-label": "Log lines" });
  const body = el("div", { class: "log-body" }, el("div", { class: "log-tools" }, filter, active), lines);
  body.hidden = true;
  root.appendChild(el("section", { class: "panel log", "aria-label": "Log" }, toggle, body));
  return { toggle, summary, filter, active, lines, body, pool: [], open: false, log: null, stick: true, needsDraw: true };
}

/** The lines whose timestamp, level or text contain `needle` (already lower-cased); all of them for "". */
function matching(lines: LogLine[], needle: string): LogLine[] {
  if (needle === "") return lines;
  return lines.filter((l) => `${l.ts} ${l.level} ${l.text}`.toLowerCase().includes(needle));
}

/** Grow or shrink the row pool to exactly `n` rows. */
function resizePool(p: Panel, n: number): void {
  while (p.pool.length < n) {
    const line = createLine();
    p.pool.push(line);
    p.lines.appendChild(line.node);
  }
  while (p.pool.length > n) p.pool.pop()?.node.remove();
}

function draw(p: Panel): void {
  p.needsDraw = false;
  if (p.log === null) return;
  const shown = matching(p.log.lines, p.filter.value.trim().toLowerCase());
  resizePool(p, shown.length);
  shown.forEach((l, i) => p.pool[i]?.set(l));
  setText(p.active, p.log.active ?? "");
  if (p.stick) p.lines.scrollTop = p.lines.scrollHeight;
}

/** Draw now if the panel is open; otherwise draw when it next opens. */
function drawOrDefer(p: Panel): void {
  if (p.open) draw(p);
  else p.needsDraw = true;
}

function onToggle(p: Panel): void {
  p.open = !p.open;
  p.toggle.setAttribute("aria-expanded", String(p.open));
  p.body.hidden = !p.open;
  if (!p.open) return;
  if (p.needsDraw) draw(p);
  p.lines.scrollTop = p.lines.scrollHeight;
}

function onScroll(p: Panel): void {
  p.stick = p.lines.scrollHeight - p.lines.scrollTop - p.lines.clientHeight < BOTTOM_SLACK_PX;
}

/** `1 error`, `2 errors`. */
function counted(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

function toneOf(errors: number, warns: number): string {
  if (errors > 0) return "error";
  if (warns > 0) return "warn";
  return "none";
}

function summarize(p: Panel, lines: LogLine[]): void {
  const errors = lines.filter((l) => l.level === "error").length;
  const warns = lines.filter((l) => l.level === "warn").length;
  const parts = [`${lines.length} lines`];
  if (errors > 0) parts.push(counted(errors, "error"));
  if (warns > 0) parts.push(counted(warns, "warning"));
  setText(p.summary, parts.join(" · "));
  p.summary.dataset["tone"] = toneOf(errors, warns);
}

function updatePanel(p: Panel, next: Log | null): void {
  if (next === null) return;
  p.log = next;
  summarize(p, next.lines);
  drawOrDefer(p);
  setHidden(p.active, next.active === null);
}

export function mount(root: HTMLElement): Mounted<Log | null> {
  const p = createPanel(root);
  p.lines.addEventListener("scroll", () => onScroll(p));
  p.toggle.addEventListener("click", () => onToggle(p));
  p.filter.addEventListener("input", () => drawOrDefer(p));
  return { update: (next) => updatePanel(p, next) };
}
