// What answering a question sends: the one intent that names the question and
// the answer the human chose. Pure, so it is tested without a document.

import type { Gate, GateOption, WireIntent, WireLinkAnswer, WireToolAnswer, WireWebAnswer } from "./wire.ts";

const LINK_ANSWERS: readonly string[] = ["deny", "session"] satisfies WireLinkAnswer[];

/**
 * The intent for `option` on `gate`, or null when it is not this surface's to
 * send: a granting answer the server marked as not allowed here (a saved one,
 * SPEC R-GUI.9), or, for a hooked command's link, anything but deny and this
 * site for this session (R-LINK.6). The server refuses those anyway; the page
 * does not ask.
 */
export function intentFor(gate: Gate, option: GateOption): WireIntent | null {
  if (option.granting && !option.allowed) return null;
  switch (gate.kind) {
    case "scope":
      return { type: "answer_scope_grant", decision_id: gate.decision_id, option: option.value };
    case "web":
      return { type: "answer_web_approval", decision_id: gate.decision_id, answer: option.value as WireWebAnswer };
    case "link":
      return LINK_ANSWERS.includes(option.value)
        ? { type: "answer_link_ask", decision_id: gate.decision_id, answer: option.value as WireLinkAnswer }
        : null;
    case "tool":
      return { type: "answer_tool_approval", op_id: gate.op_id, answer: option.value as WireToolAnswer };
  }
}
