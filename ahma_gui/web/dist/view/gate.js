// The pinned permission question. The server decides what is asked and which
// answers are offered, in what order, and which are allowed; this draws exactly
// that and sends back the one the human chose.
//
// What binds this file (the human is being asked to widen a sandbox):
//  - No granting answer is ever the default. Nothing here calls focus(), so no
//    granting button is focused until the human moves to it; one answers only a
//    trusted activation (a click, or Space/Enter on the button they chose), never
//    a script's, and not in the moment after the question (re)draws. Keyboard and
//    assistive-technology users can grant as well as deny.
//  - Deny answers are always enabled, visually primary, and Enter/Escape inside
//    the panel (not on a granting button) choose the first of them.
//  - A scope, web or link question tells the server whether it is being
//    *read*: visible, focused document. Hidden is reported the moment that stops being true.
//  - The question stays until the server's view no longer has it.
import { send } from "../act.js";
import { intentFor } from "../answer.js";
import { postReport } from "../api.js";
import { el, setHidden, setText } from "../dom.js";
/** A click this soon after the question (re)drew is not a considered one. */
const SETTLE_MS = 500;
const BASE_TITLE = "ahma";
const TITLE_PREFIX = "● Question — ";
function idOf(gate) {
    return gate.kind === "tool" ? gate.op_id : gate.decision_id;
}
/** Everything that decides how the panel is built; allowed/hint are patched in place. */
function structureOf(gate) {
    return JSON.stringify({
        ...gate,
        options: gate.options.map((o) => ({ value: o.value, label: o.label, granting: o.granting })),
    });
}
function section(heading, body, mono = false) {
    return el("div", { class: "gate-section" }, heading !== null && el("div", { class: "gate-heading" }, heading), el("div", { class: mono ? "gate-body mono" : "gate-body" }, body));
}
const advice = (text) => text === null ? null : el("p", { class: "gate-advice quiet" }, text);
/** The body parts each kind of question has, a null for one it leaves out. */
function partsFor(g) {
    switch (g.kind) {
        case "scope":
            return [...g.sections.map((s) => section(s.heading, s.body)), advice(g.advice)];
        case "web":
            return [section("URL", g.url, true), g.tool === null ? null : section("Requested by", g.tool, true)];
        case "link":
            return [section("Refused in", g.workspace, true), section(null, g.text), advice(g.note)];
        case "tool":
            return [section(null, g.description), advice(g.note)];
    }
}
/** What the question says, apart from its title and its buttons. */
function bodyFor(g) {
    return partsFor(g).filter((p) => p !== null);
}
function kickerFor(g) {
    return { scope: "Sandbox access", web: "Network request", link: "Open a web link", tool: "Tool approval" }[g.kind];
}
function titleFor(g) {
    switch (g.kind) {
        case "scope":
            return g.title;
        case "web":
            return g.domain;
        case "link":
            return g.site;
        case "tool":
            return g.tool;
    }
}
function makeOption(option, onClick) {
    const button = el("button", {
        type: "button",
        class: option.granting ? "btn btn-grant" : "btn btn-primary",
        "data-granting": String(option.granting),
    }, option.label);
    button.addEventListener("click", (event) => onClick(event, option));
    return { option, button, hint: el("span", { class: "opt-hint quiet" }) };
}
/** Bring one drawn option up to date with the server's: whether it is allowed, and its hint. */
function patchOption(o, option) {
    o.option = option;
    const disabled = option.granting && !option.allowed; // a deny is never disabled
    if (o.button.disabled !== disabled)
        o.button.disabled = disabled;
    setText(o.hint, option.hint ?? "");
    setHidden(o.hint, option.hint === null);
}
function patchOptions(options, g) {
    g.options.forEach((option, i) => {
        const o = options[i];
        if (o)
            patchOption(o, option);
    });
}
function report(s, r) {
    s.queue = s.queue.then(() => postReport(r)).catch(() => { });
}
/** The question on screen when the human can be reading it (visible, focused document), else null. */
function readId(s) {
    if (s.reportId === null)
        return null;
    return document.visibilityState === "visible" && document.hasFocus() ? s.reportId : null;
}
/** Tell the server what changed: hidden for one no longer read, then visible for the one now read. */
function evaluate(s) {
    const read = readId(s);
    if (s.reported !== null && s.reported !== read) {
        report(s, { type: "gate_hidden", decision_id: s.reported });
        s.reported = null;
    }
    if (read !== null && s.reported === null) {
        s.reported = read;
        report(s, { type: "gate_visible", decision_id: read });
    }
}
function createReadReporter() {
    const s = { reportId: null, reported: null, queue: Promise.resolve() };
    const onChange = () => evaluate(s);
    const show = (id) => {
        s.reportId = id;
        evaluate(s);
    };
    const onPageHide = () => show(null);
    const triggers = [
        [document, "visibilitychange"],
        [window, "focus"],
        [window, "blur"],
    ];
    for (const [target, type] of triggers)
        target.addEventListener(type, onChange);
    window.addEventListener("pagehide", onPageHide);
    return {
        show,
        resync() {
            if (s.reported !== null)
                report(s, { type: "gate_visible", decision_id: s.reported });
            else
                evaluate(s);
        },
        destroy() {
            for (const [target, type] of triggers)
                target.removeEventListener(type, onChange);
            window.removeEventListener("pagehide", onPageHide);
            show(null);
        },
    };
}
function createPanel(root) {
    const kicker = el("span", { class: "gate-kicker" });
    const count = el("span", { class: "gate-count quiet" });
    const content = el("div", { class: "gate-content" });
    const panel = el("section", { class: "gate", role: "alertdialog", tabindex: 0, "aria-labelledby": "gate-title" }, el("div", { class: "gate-head" }, kicker, count), content);
    panel.hidden = true;
    root.appendChild(panel);
    const reporter = createReadReporter();
    return { panel, kicker, count, content, reporter, gate: null, structure: "", options: [], shownAt: 0, answering: false };
}
// ── answering ──
async function answer(p, g, option) {
    if (p.answering)
        return; // one request at a time; the server's view decides what happens next
    const intent = intentFor(g, option);
    if (intent === null)
        return; // not this surface's to send; the button says where it is
    p.answering = true;
    try {
        await send(intent);
    }
    finally {
        p.answering = false;
    }
}
function deny(p) {
    const g = p.gate;
    const option = g?.options.find((o) => !o.granting);
    if (g && option)
        void answer(p, g, option);
}
/** A script cannot make a trusted event; a human at the keyboard or a pointer can. */
function isConsidered(p, event) {
    return event.isTrusted && performance.now() - p.shownAt >= SETTLE_MS;
}
function onOptionClick(p, event, g, option) {
    if (option.granting && !isConsidered(p, event))
        return;
    void answer(p, g, option);
}
/** Escape denies, and so does Enter when focus is in the panel but on no button. */
function isDenyKey(event) {
    if (event.key === "Escape")
        return true;
    if (event.key !== "Enter")
        return false;
    // Enter on a button activates that button, as a keyboard user expects.
    return !(event.target instanceof Element && event.target.closest("button") !== null);
}
function onKeydown(p, event) {
    if (!isDenyKey(event))
        return;
    event.preventDefault();
    if (!event.repeat)
        deny(p);
}
// ── drawing ──
function build(p, g) {
    p.shownAt = performance.now();
    p.options = g.options.map((option) => makeOption(option, (event, o) => onOptionClick(p, event, g, o)));
    setText(p.kicker, kickerFor(g));
    p.content.replaceChildren(el("h2", { id: "gate-title", class: g.kind === "scope" ? "gate-title" : "gate-title mono" }, titleFor(g)), ...bodyFor(g), el("div", { class: "gate-options" }, ...p.options.map((o) => el("div", { class: "opt" }, o.button, o.hint))));
}
function clear(p) {
    setHidden(p.panel, true);
    p.structure = "";
    p.options = [];
    p.content.replaceChildren();
    document.title = BASE_TITLE;
    p.reporter.show(null);
}
/** Draw `top`, rebuilding only when it is a new question or its structure changed. */
function present(p, top, previous, waiting) {
    const next = structureOf(top);
    if (previous === null || next !== p.structure) {
        p.structure = next;
        build(p, top);
    }
    patchOptions(p.options, top);
    setText(p.count, waiting > 1 ? `1 of ${waiting} waiting` : "");
    setHidden(p.panel, false);
    document.title = TITLE_PREFIX + BASE_TITLE;
    p.reporter.show(top.kind === "tool" ? null : idOf(top));
}
function updatePanel(p, gates) {
    const top = gates?.top ?? null;
    const previous = p.gate;
    p.gate = top;
    if (top === null)
        return clear(p);
    present(p, top, previous, gates?.waiting ?? 1);
}
export function mount(root) {
    const p = createPanel(root);
    p.panel.addEventListener("keydown", (event) => onKeydown(p, event));
    return {
        update: (gates) => updatePanel(p, gates),
        resync: p.reporter.resync,
        destroy() {
            p.reporter.destroy();
            document.title = BASE_TITLE;
            p.panel.remove();
        },
    };
}
