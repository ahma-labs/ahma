// The page's state is the latest copy of each region the server has sent. The
// reducer is pure: a View merges in region by region, whole regions replacing
// whole regions (never a deep merge), and a region the View leaves out keeps its
// previous object, so "did this region change" is a reference comparison.

import { PROTOCOL } from "./wire.ts";
import type { Chat, Gates, Header, Log, View, Work } from "./wire.ts";

export interface State {
  rev: number;
  header: Header | null;
  work: Work | null;
  chat: Chat | null;
  log: Log | null;
  gates: Gates | null;
  /** The protocol the server announced when it was not ours; null while fine. */
  protocolError: number | null;
}

export type Region = "header" | "work" | "chat" | "log" | "gates";
export const REGIONS: readonly Region[] = ["header", "work", "chat", "log", "gates"];

export const initialState: State = {
  rev: 0,
  header: null,
  work: null,
  chat: null,
  log: null,
  gates: null,
  protocolError: null,
};

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === "object" && x !== null && !Array.isArray(x);
}

/**
 * Merge one server message into the state. Anything that is not a View is
 * ignored; a View of another protocol changes nothing but `protocolError`.
 */
export function reduce(state: State, message: unknown): State {
  if (!isRecord(message) || typeof message["protocol"] !== "number") return state;
  if (message["protocol"] !== PROTOCOL) {
    return state.protocolError === message["protocol"]
      ? state
      : { ...state, protocolError: message["protocol"] };
  }
  const view = message as unknown as View;
  const next: State = { ...state, rev: typeof view.rev === "number" ? view.rev : state.rev };
  for (const region of REGIONS) {
    const incoming = view[region];
    if (isRecord(incoming)) (next as unknown as Record<Region, unknown>)[region] = incoming;
  }
  return next;
}

/** The regions whose object differs between two states. */
export function changedRegions(a: State, b: State): Region[] {
  return REGIONS.filter((r) => a[r] !== b[r]);
}
