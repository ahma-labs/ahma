import assert from "node:assert/strict";
import { test } from "node:test";
import { formatDuration, shortenPath } from "../src/format.ts";
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
