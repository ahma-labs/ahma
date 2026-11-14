import assert from "node:assert/strict";
import { test } from "node:test";
import { createSseParser } from "../src/sse.ts";
import type { SseEvent } from "../src/sse.ts";

/** Feed `input` in chunks of `size` characters and collect every event. */
function parse(input: string, size = input.length): SseEvent[] {
  const p = createSseParser();
  const out: SseEvent[] = [];
  for (let i = 0; i < input.length; i += size) out.push(...p.feed(input.slice(i, i + size)));
  return out;
}

const ev = (event: string, data: string): SseEvent => ({ event, data });

test("a complete event", () => {
  assert.deepEqual(parse("event: view\ndata: {\"a\":1}\n\n"), [ev("view", '{"a":1}')]);
});

test("an event without a name is a message", () => {
  assert.deepEqual(parse("data: hi\n\n"), [ev("message", "hi")]);
});

test("the same bytes give the same events however they are chunked", () => {
  const input = "event: view\ndata: one\n\n: keepalive\n\nevent: bye\ndata: \n\n";
  const whole = parse(input);
  for (let size = 1; size <= input.length; size++) assert.deepEqual(parse(input, size), whole, `chunk size ${size}`);
  assert.deepEqual(whole, [ev("view", "one"), ev("bye", "")]);
});

test("CRLF line ends, including a \\r and \\n split across chunks", () => {
  const input = "event: view\r\ndata: x\r\n\r\nevent: view\r\ndata: y\r\n\r\n";
  assert.deepEqual(parse(input), [ev("view", "x"), ev("view", "y")]);
  assert.deepEqual(parse(input, 1), [ev("view", "x"), ev("view", "y")]);
  const p = createSseParser();
  assert.deepEqual(p.feed("data: a\r"), []);
  assert.deepEqual(p.feed("\n\r"), [ev("message", "a")]); // the \n finished the first line; this \r ends the event
  assert.deepEqual(p.feed("\ndata: b\r\r"), [ev("message", "b")]); // and a \n after that \r belongs to it
});

test("lone CR is a line end", () => {
  assert.deepEqual(parse("data: a\r\rdata: b\r\r"), [ev("message", "a"), ev("message", "b")]);
});

test("comment lines are ignored, even in the middle of an event", () => {
  assert.deepEqual(parse(": hello\nevent: view\n: mid\ndata: x\n\n"), [ev("view", "x")]);
  assert.deepEqual(parse(": only a comment\n\n"), []);
});

test("multi-line data is joined with newlines", () => {
  assert.deepEqual(parse("data: a\ndata: b\ndata:\ndata: c\n\n"), [ev("message", "a\nb\n\nc")]);
});

test("only one leading space after the colon is dropped", () => {
  assert.deepEqual(parse("data:  two\n\ndata:none\n\n"), [ev("message", " two"), ev("message", "none")]);
});

test("a field with no colon is the field name with an empty value", () => {
  assert.deepEqual(parse("data\n\n"), [ev("message", "")]);
});

test("a blank line with no data dispatches nothing and resets the event name", () => {
  assert.deepEqual(parse("event: view\n\ndata: x\n\n"), [ev("message", "x")]);
});

test("an event is not delivered until its blank line arrives", () => {
  const p = createSseParser();
  assert.deepEqual(p.feed("event: view\ndata: x\n"), []);
  assert.deepEqual(p.feed("\n"), [ev("view", "x")]);
});

test("a leading BOM is skipped", () => {
  assert.deepEqual(parse("\ufeffdata: x\n\n"), [ev("message", "x")]);
});

test("unknown fields (id, retry) are ignored", () => {
  assert.deepEqual(parse("id: 7\nretry: 100\nevent: view\ndata: x\n\n"), [ev("view", "x")]);
});

test("a stream that never ends a line is refused rather than buffered forever", () => {
  const p = createSseParser();
  const chunk = "x".repeat(1024 * 1024);
  assert.throws(() => {
    for (let i = 0; i < 20; i++) p.feed(chunk);
  }, /too long/);
});
