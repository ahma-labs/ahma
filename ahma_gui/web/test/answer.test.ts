import assert from "node:assert/strict";
import { test } from "node:test";
import { intentFor } from "../src/answer.ts";
import type { Gate, GateOption } from "../src/wire.ts";

const opt = (value: string, granting: boolean, allowed = true): GateOption => ({
  value,
  label: value,
  granting,
  allowed,
  hint: null,
});

const link: Gate = {
  kind: "link",
  decision_id: "harness-link-1",
  site: "github.com",
  workspace: "/w/a",
  text: "A command wants to open a web link in your browser.",
  note: "The command was not paused.",
  options: [opt("deny", false), opt("session", true), opt("always-site", true, false)],
};

// SPEC R-LINK.6, R-GUI.9: a hooked command's refused link is answered by its
// own intent, naming the question and the answer the human chose.
test("a link question's answer names the question and the answer", () => {
  assert.deepEqual(intentFor(link, opt("deny", false)), {
    type: "answer_link_ask",
    decision_id: "harness-link-1",
    answer: "deny",
  });
  assert.deepEqual(intentFor(link, opt("session", true)), {
    type: "answer_link_ask",
    decision_id: "harness-link-1",
    answer: "session",
  });
});

// A saving answer is shown, disabled, to say where it is given; it is never sent.
test("an answer this surface may not give is never sent", () => {
  assert.equal(intentFor(link, opt("always-site", true, false)), null);
  assert.equal(intentFor(link, opt("once", true)), null, "a hooked command does not wait");
});

test("the other questions keep their intents", () => {
  const web: Gate = { kind: "web", decision_id: "w", domain: "example.com", url: "https://example.com/", tool: null, options: [] };
  assert.deepEqual(intentFor(web, opt("session", true)), { type: "answer_web_approval", decision_id: "w", answer: "session" });
  const scope: Gate = { kind: "scope", decision_id: "s", title: "t", sections: [], options: [], advice: null };
  assert.deepEqual(intentFor(scope, opt("read-write-session", true)), {
    type: "answer_scope_grant",
    decision_id: "s",
    option: "read-write-session",
  });
  const tool: Gate = { kind: "tool", op_id: "o", tool: "x", description: "d", note: null, options: [] };
  assert.deepEqual(intentFor(tool, opt("reject", false)), { type: "answer_tool_approval", op_id: "o", answer: "reject" });
});
