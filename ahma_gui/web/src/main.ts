// Boot: pair, then run the session. The server owns every decision; this file
// only wires the transport (stream.ts) into the store (store.ts) and the store
// into the views, drawing each region once per frame and only if it changed.

import { forgetToken, pair, setOnUnauthorized } from "./api.ts";
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
  showPair(notice);
}

function startSession(): void {
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

  const toast = toastView.mount(app);
  const header = headerView.mount(headerRoot);
  const gate = gateView.mount(gateRoot);
  const work = workView.mount(workRoot);
  const chat = chatView.mount(chatRoot);
  const log = logView.mount(logRoot);

  let state: State = initialState;
  let shown: State = { ...initialState, rev: -1 };
  let link: StreamState = "connecting";
  let shownLink: StreamState | null = null;
  let pending = false;
  let stopped = false;

  function draw(): void {
    pending = false;
    if (stopped) return;
    const next = state;
    const changed = new Set(changedRegions(shown, next));
    shown = next;
    // A view that throws must not take the others (above all the gate) with it.
    const run = <T>(view: Mounted<T>, data: T): void => {
      try {
        view.update(data);
      } catch (err) {
        console.error(err);
        toastView.notify("Part of the page could not be drawn.");
      }
    };
    if (changed.has("header") || link !== shownLink) run(header, { header: next.header, link });
    shownLink = link;
    if (changed.has("gates")) run(gate, next.gates);
    if (changed.has("work")) run(work, next.work);
    if (changed.has("chat")) run(chat, next.chat);
    if (changed.has("log")) run(log, next.log);
  }

  function schedule(): void {
    if (pending || stopped) return;
    pending = true;
    // Frames do not run in a hidden tab, and that is exactly when a new question
    // must still reach the title bar.
    if (document.visibilityState === "visible") requestAnimationFrame(draw);
    else setTimeout(draw, 0);
  }

  const stop = startStream({
    onView(message) {
      state = reduce(state, message);
      if (state.protocolError !== null) {
        if (stopped) return;
        end();
        showFatal(`This page speaks protocol ${PROTOCOL}; the server speaks protocol ${state.protocolError}. Reload after updating ahma.`);
        return;
      }
      schedule();
    },
    onState(next) {
      if (next === "live" && link !== "live") gate.resync();
      link = next;
      if (next === "unauthorized") {
        lost();
        return;
      }
      schedule();
    },
  });

  setOnUnauthorized(lost);

  /** The server no longer knows this token: back to pairing, once. */
  function lost(): void {
    if (stopped) return;
    end();
    endSession("Your session ended. Enter a new pairing code.");
  }

  function end(): void {
    if (stopped) return;
    stopped = true;
    stop();
  }

  show({
    destroy() {
      end();
      for (const view of [toast, header, gate, work, chat, log]) view.destroy?.();
      app.remove();
    },
  });
  schedule();
}

showPair(null);
