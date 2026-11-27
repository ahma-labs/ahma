// Boot: pair, then run the session. The server owns every decision; this file
// only wires the transport (stream.ts) into the store (store.ts) and the store
// into the views, drawing each region once per frame and only if it changed.

import { adoptToken, desktopToken, forgetToken, pair, setOnUnauthorized } from "./api.ts";
import { el } from "./dom.ts";
import type { Mounted } from "./dom.ts";
import { startStream } from "./stream.ts";
import type { StreamState } from "./stream.ts";
import { changedRegions, initialState, reduce } from "./store.ts";
import type { State } from "./store.ts";
import * as chatView from "./view/chat.ts";
import * as gateView from "./view/gate.ts";
import * as headerView from "./view/header.ts";
import * as logView from "./view/log.ts";
import * as pairView from "./view/pair.ts";
import * as toastView from "./view/toast.ts";
import * as workView from "./view/work.ts";
import { PROTOCOL } from "./wire.ts";

const rootEl = document.getElementById("root");
if (rootEl === null) throw new Error("no #root");
const root: HTMLElement = rootEl;

let teardown: (() => void) | null = null;

function show(screen: { destroy?: (() => void) | undefined }): void {
  teardown?.();
  teardown = () => screen.destroy?.();
}

function showPair(notice: string | null): void {
  const screen = pairView.mount(root, {
    submit: async (code) => {
      await pair(code);
      startSession();
    },
  });
  screen.update({ notice });
  show(screen);
}

function showFatal(message: string): void {
  const card = el("main", { class: "pair" }, el("div", { class: "pair-card" }, el("h1", { class: "pair-title" }, "ahma"), el("p", { class: "pair-message", role: "alert" }, message)));
  root.replaceChildren(card);
  show({});
}

function endSession(notice: string): void {
  forgetToken();
  if (linked) {
    // A desktop window has no code to pair with: its session is its life.
    showFatal("This window's session has ended. Close the window and run ahma desktop again.");
    return;
  }
  showPair(notice);
}

// ── the session ──

interface Views {
  toast: ReturnType<typeof toastView.mount>;
  header: ReturnType<typeof headerView.mount>;
  gate: gateView.GateView;
  work: ReturnType<typeof workView.mount>;
  chat: ReturnType<typeof chatView.mount>;
  log: ReturnType<typeof logView.mount>;
}

interface Session {
  readonly views: Views;
  /** The newest state the stream has given. */
  state: State;
  /** The state the views last drew. */
  shown: State;
  link: StreamState;
  /** The link state the header last drew, null before the first draw. */
  shownLink: StreamState | null;
  /** A draw is scheduled and has not run yet. */
  pending: boolean;
  stopped: boolean;
  /** Stops the stream; set once the stream starts. */
  stop: () => void;
}

/** Lay out the session's regions in `root` and mount a view in each. */
function mountSession(): { app: HTMLElement; views: Views } {
  const app = el("div", { class: "app" });
  const top = el("div", { class: "top" });
  const headerRoot = el("div", {});
  const gateRoot = el("div", {});
  const workRoot = el("div", {});
  const chatRoot = el("div", {});
  const logRoot = el("div", {});
  top.append(headerRoot, gateRoot);
  app.append(top, el("main", { class: "main" }, workRoot, chatRoot), logRoot);
  root.replaceChildren(app);

  return {
    app,
    views: {
      toast: toastView.mount(app),
      header: headerView.mount(headerRoot),
      gate: gateView.mount(gateRoot),
      work: workView.mount(workRoot),
      chat: chatView.mount(chatRoot),
      log: logView.mount(logRoot),
    },
  };
}

/** A view that throws must not take the others (above all the gate) with it. */
function drawView<T>(view: Mounted<T>, data: T): void {
  try {
    view.update(data);
  } catch (err) {
    console.error(err);
    toastView.notify("Part of the page could not be drawn.");
  }
}

function draw(s: Session): void {
  s.pending = false;
  if (s.stopped) return;
  const next = s.state;
  const changed = new Set(changedRegions(s.shown, next));
  s.shown = next;
  const { header, gate, work, chat, log } = s.views;
  if (changed.has("header") || s.link !== s.shownLink) drawView(header, { header: next.header, link: s.link });
  s.shownLink = s.link;
  if (changed.has("gates")) drawView(gate, next.gates);
  if (changed.has("work")) drawView(work, next.work);
  if (changed.has("chat")) drawView(chat, next.chat);
  if (changed.has("log")) drawView(log, next.log);
}

function schedule(s: Session): void {
  if (s.pending || s.stopped) return;
  s.pending = true;
  const frame = (): void => draw(s);
  // Frames do not run in a hidden tab, and that is exactly when a new question
  // must still reach the title bar.
  if (document.visibilityState === "visible") requestAnimationFrame(frame);
  else setTimeout(frame, 0);
}

function end(s: Session): void {
  if (s.stopped) return;
  s.stopped = true;
  s.stop();
}

/** The server no longer knows this token: back to pairing, once. */
function lost(s: Session): void {
  if (s.stopped) return;
  end(s);
  endSession("Your session ended. Enter a new pairing code.");
}

function onView(s: Session, message: unknown): void {
  s.state = reduce(s.state, message);
  if (s.state.protocolError === null) return schedule(s);
  if (s.stopped) return;
  end(s);
  showFatal(`This page speaks protocol ${PROTOCOL}; the server speaks protocol ${s.state.protocolError}. Reload after updating ahma.`);
}

function onState(s: Session, next: StreamState): void {
  if (next === "live" && s.link !== "live") s.views.gate.resync();
  s.link = next;
  if (next === "unauthorized") return lost(s);
  schedule(s);
}

function startSession(): void {
  const { app, views } = mountSession();
  const s: Session = {
    views,
    state: initialState,
    shown: { ...initialState, rev: -1 },
    link: "connecting",
    shownLink: null,
    pending: false,
    stopped: false,
    stop: () => {},
  };

  // The stream reports "connecting" before startStream returns; nothing calls
  // stop() that early, so the placeholder above is never what is stopped.
  s.stop = startStream({
    onView: (message) => onView(s, message),
    onState: (next) => onState(s, next),
  });

  setOnUnauthorized(() => lost(s));

  show({
    destroy() {
      end(s);
      const { toast, header, gate, work, chat, log } = views;
      for (const view of [toast, header, gate, work, chat, log]) view.destroy?.();
      app.remove();
    },
  });
  schedule(s);
}

// `ahma desktop`'s own window arrives already holding its token.
const given = desktopToken();
const linked = given !== null;
if (given !== null) {
  adoptToken(given);
  startSession();
} else {
  showPair(null);
}
