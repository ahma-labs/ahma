import assert from "node:assert/strict";
import { test } from "node:test";
import { formatDuration, looksLikeFullCode, shortenPath } from "../src/format.ts";
import { backoffDelay } from "../src/stream.ts";

test("paths are shortened to their last segments", () => {
  assert.equal(shortenPath("/Users/me/code/project"), "…/code/project");
  assert.equal(shortenPath("C:\\Users\\me\\code"), "…/me/code");
  assert.equal(shortenPath("/a/b"), "/a/b");
  assert.equal(shortenPath("/"), "/");
});

test("durations read naturally", () => {
  assert.equal(formatDuration(850), "850ms");
  assert.equal(formatDuration(1234), "1.2s");
  assert.equal(formatDuration(42_000), "42s");
  assert.equal(formatDuration(184_000), "3m 04s");
  assert.equal(formatDuration(3_720_000), "1h 02m");
});

test("reconnect backoff grows and is capped at about five seconds", () => {
  const mid = (n: number) => backoffDelay(n, 0.5);
  assert.equal(mid(0), 250);
  assert.ok(mid(1) > mid(0) && mid(2) > mid(1));
  for (let n = 0; n < 40; n++) {
    assert.ok(backoffDelay(n, 1) <= 5000);
    assert.ok(backoffDelay(n, 0) >= 200);
  }
  assert.equal(mid(30), 5000);
});

// A pasted code is submitted at once; a half-typed one is not. The reading
// mirrors the server's (auth.rs `normalize`): case, spaces and the dash do not
// matter.
test("a whole pairing code is recognised the way the server reads it", () => {
  assert.equal(looksLikeFullCode("ABCD-1234"), true);
  assert.equal(looksLikeFullCode(" abcd 1234\n"), true);
  assert.equal(looksLikeFullCode("ABCD-123"), false);
  assert.equal(looksLikeFullCode("ABCD-12345"), false);
  assert.equal(looksLikeFullCode("ABCD-12!4"), false);
});
