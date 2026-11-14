import assert from "node:assert/strict";
import { test } from "node:test";
import { changedRegions, initialState, reduce } from "../src/store.ts";
import type { Chat, Gates, Header, Work } from "../src/wire.ts";

const header = (workspace: string): Header => ({
  workspace,
  hub: true,
  server: true,
  transport: "stdio",
  sandbox: { label: "kernel sandbox", enforced: true, sole_authority: true, disclosure: null, note: null, write: [], read: [] },
  model: null,
  counts: { running: 0, queued: 0, succeeded: 0, failed: 0 },
});
const work = (_n: number): Work => ({ all_projects: false, project: null, sections: [] });
const chat = (ids: number[]): Chat => ({ entries: ids.map((id) => ({ kind: "notice" as const, id, text: `n${id}` })), turn: null, target: null });
const gates = (waiting: number): Gates => ({ waiting, top: null });

test("the first view carries every region", () => {
  const s = reduce(initialState, { protocol: 1, rev: 1, header: header("/a"), work: work(0), chat: chat([1]), gates: gates(0) });
  assert.equal(s.rev, 1);
  assert.equal(s.header?.workspace, "/a");
  assert.equal(s.chat?.entries.length, 1);
  assert.equal(s.log, null, "a region never sent stays empty");
  assert.equal(s.protocolError, null);
});

test("a partial view replaces only the regions it carries", () => {
  const first = reduce(initialState, { protocol: 1, rev: 1, header: header("/a"), work: work(0), chat: chat([1]) });
  const second = reduce(first, { protocol: 1, rev: 2, chat: chat([1, 2]) });
  assert.equal(second.rev, 2);
  assert.equal(second.header, first.header, "an absent region keeps the very same object");
  assert.equal(second.work, first.work);
  assert.notEqual(second.chat, first.chat);
  assert.deepEqual(changedRegions(first, second), ["chat"]);
});

test("a region is replaced whole, never deep-merged", () => {
  const first = reduce(initialState, { protocol: 1, rev: 1, chat: { entries: [{ kind: "notice", id: 1, text: "x" }], turn: { phase: "thinking", tool: null }, target: "w" } });
  const second = reduce(first, { protocol: 1, rev: 2, chat: { entries: [], turn: null, target: null } });
  assert.deepEqual(second.chat, { entries: [], turn: null, target: null });
});

test("a gate that the server clears is cleared", () => {
  const withGate = reduce(initialState, {
    protocol: 1,
    rev: 1,
    gates: { waiting: 1, top: { kind: "tool", op_id: "o", tool: "t", description: "d", note: null, options: [] } },
  });
  assert.equal(withGate.gates?.top?.kind, "tool");
  const cleared = reduce(withGate, { protocol: 1, rev: 2, gates: { waiting: 0, top: null } });
  assert.deepEqual(cleared.gates, { waiting: 0, top: null });
});

test("a protocol mismatch changes nothing but the error", () => {
  const ok = reduce(initialState, { protocol: 1, rev: 5, header: header("/a") });
  const bad = reduce(ok, { protocol: 2, rev: 6, header: header("/b"), gates: gates(3) });
  assert.equal(bad.protocolError, 2);
  assert.equal(bad.rev, 5);
  assert.equal(bad.header, ok.header);
  assert.equal(bad.gates, ok.gates);
  assert.equal(reduce(bad, { protocol: 2, rev: 7 }), bad, "repeating it is not a new state");
});

test("what is not a view is ignored", () => {
  for (const junk of [null, undefined, 7, "view", [], {}, { rev: 1 }, { protocol: "1" }]) {
    assert.equal(reduce(initialState, junk), initialState, JSON.stringify(junk));
  }
});

test("a region of the wrong type is ignored rather than stored", () => {
  const s = reduce(initialState, { protocol: 1, rev: 1, header: "nope", work: null, chat: [1] });
  assert.equal(s.header, null);
  assert.equal(s.work, null);
  assert.equal(s.chat, null);
  assert.equal(s.rev, 1);
});

test("a server that restarts (rev goes backwards) is still believed", () => {
  const a = reduce(initialState, { protocol: 1, rev: 40, header: header("/a") });
  const b = reduce(a, { protocol: 1, rev: 1, header: header("/b") });
  assert.equal(b.rev, 1);
  assert.equal(b.header?.workspace, "/b");
});

test("the reducer does not mutate its input", () => {
  const before = reduce(initialState, { protocol: 1, rev: 1, header: header("/a") });
  const snapshot = JSON.stringify(before);
  reduce(before, { protocol: 1, rev: 2, chat: chat([1]) });
  assert.equal(JSON.stringify(before), snapshot);
  assert.equal(initialState.header, null);
});
