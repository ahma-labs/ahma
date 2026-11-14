// The live view: GET /api/events read with fetch (the built-in SSE client cannot send the
// Authorization header) and framed by the pure parser in sse.ts. A dropped
// stream reconnects with a bounded exponential backoff; the server's `bye` stops
// it for good; a 401 hands control back to pairing.

import { ApiError, openEvents } from "./api.ts";
import { createSseParser } from "./sse.ts";

export type StreamState = "connecting" | "live" | "reconnecting" | "closed" | "unauthorized";

export interface StreamHandlers {
  /** One parsed `view` event, unvalidated: the store decides what it is. */
  onView(message: unknown): void;
  onState(state: StreamState): void;
}

const BACKOFF_BASE_MS = 250;
const BACKOFF_CAP_MS = 5000;

export function backoffDelay(attempt: number, random: number = Math.random()): number {
  const base = Math.min(BACKOFF_CAP_MS, BACKOFF_BASE_MS * 2 ** attempt);
  return Math.min(BACKOFF_CAP_MS, Math.round(base * (0.8 + 0.4 * random)));
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

/** Start reading; the returned function stops it. */
export function startStream(handlers: StreamHandlers): () => void {
  const controller = new AbortController();
  void run(handlers, controller.signal);
  return () => controller.abort();
}

async function run(h: StreamHandlers, signal: AbortSignal): Promise<void> {
  let attempt = 0;
  h.onState("connecting");
  while (!signal.aborted) {
    try {
      const res = await openEvents(signal);
      if (res.body === null) throw new Error("no body");
      h.onState("live");
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      const parser = createSseParser();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        for (const event of parser.feed(decoder.decode(value, { stream: true }))) {
          if (event.event === "bye") {
            h.onState("closed");
            return;
          }
          if (event.event !== "view") continue;
          let message: unknown;
          try {
            message = JSON.parse(event.data);
          } catch {
            continue; // a malformed event is dropped; the next one replaces it
          }
          attempt = 0; // a stream that delivers is healthy; only a failing one backs off
          h.onView(message);
        }
      }
    } catch (err) {
      if (signal.aborted) return;
      if (err instanceof ApiError && err.status === 401) {
        h.onState("unauthorized");
        return;
      }
    }
    if (signal.aborted) return;
    h.onState("reconnecting");
    await sleep(backoffDelay(attempt++), signal);
  }
}
