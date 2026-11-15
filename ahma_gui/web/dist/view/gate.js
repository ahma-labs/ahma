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
//  - A scope/web question tells the server whether it is being *read*: visible,
//    focused document. Hidden is reported the moment that stops being true.
//  - The question stays until the server's view no longer has it.
import { postReport } from "../api.js";
import { send } from "../act.js";
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
function intentFor(gate, option) {
    switch (gate.kind) {
        case "scope":
            return { type: "answer_scope_grant", decision_id: gate.decision_id, option: option.value };
        case "web":
            return { type: "answer_web_approval", decision_id: gate.decision_id, answer: option.value };
        case "tool":
            return { type: "answer_tool_approval", op_id: gate.op_id, answer: option.value };
    }
}
function section(heading, body, mono = false) {
    return el("div", { class: "gate-section" }, heading !== null && el("div", { class: "gate-heading" }, heading), el("div", { class: mono ? "gate-body mono" : "gate-body" }, body));
}
const advice = (text) => text === null ? null : el("p", { class: "gate-advice quiet" }, text);
/** What the question says, apart from its title and its buttons. */
function bodyFor(g) {
    const parts = (() => {
        switch (g.kind) {
            case "scope":
                return [...g.sections.map((s) => section(s.heading, s.body)), advice(g.advice)];
            case "web":
                return [section("URL", g.url, true), g.tool === null ? null : section("Requested by", g.tool, true)];
            case "tool":
                return [section(null, g.description), advice(g.note)];
        }
    })();
    return parts.filter((p) => p !== null);
}
function kickerFor(g) {
    return { scope: "Sandbox access", web: "Network request", tool: "Tool approval" }[g.kind];
}
function titleFor(g) {
    return g.kind === "scope" ? g.title : g.kind === "web" ? g.domain : g.tool;
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
function createReadReporter() {
    let reportId = null; // the scope/web question on screen, if any
    let reported = null; // the one the server was last told is visible
    let queue = Promise.resolve();
    function report(r) {
        queue = queue.then(() => postReport(r)).catch(() => { });
    }
    function evaluate() {
        const readable = reportId !== null && document.visibilityState === "visible" && document.hasFocus();
        if (reported !== null && (!readable || reported !== reportId)) {
            report({ type: "gate_hidden", decision_id: reported });
            reported = null;
        }
        if (readable && reported === null && reportId !== null) {
            reported = reportId;
            report({ type: "gate_visible", decision_id: reportId });
        }
    }
    const triggers = [
        [document, "visibilitychange"],
        [window, "focus"],
        [window, "blur"],
    ];
    for (const [target, type] of triggers)
        target.addEventListener(type, evaluate);
    const onPageHide = () => show(null);
    window.addEventListener("pagehide", onPageHide);
    function show(id) {
        reportId = id;
        evaluate();
    }
    return {
        show,
        resync() {
            if (reported !== null)
                report({ type: "gate_visible", decision_id: reported });
            else
                evaluate();
        },
        destroy() {
            for (const [target, type] of triggers)
                target.removeEventListener(type, evaluate);
            window.removeEventListener("pagehide", onPageHide);
            show(null);
        },
    };
}
export function mount(root) {
    const kicker = el("span", { class: "gate-kicker" });
    const count = el("span", { class: "gate-count quiet" });
    const content = el("div", { class: "gate-content" });
    const panel = el("section", { class: "gate", role: "alertdialog", tabindex: 0, "aria-labelledby": "gate-title" }, el("div", { class: "gate-head" }, kicker, count), content);
    panel.hidden = true;
    root.appendChild(panel);
    const reporter = createReadReporter();
    let gate = null;
    let structure = "";
    let options = [];
    let shownAt = 0;
    let answering = false;
    // ── answering ──
    function deny() {
        const option = gate?.options.find((o) => !o.granting);
        if (gate && option)
            void answer(gate, option);
    }
    async function answer(g, option) {
        if (answering)
            return; // one request at a time; the server's view decides what happens next
        answering = true;
        try {
            await send(intentFor(g, option));
        }
        finally {
            answering = false;
        }
    }
    /** A script cannot make a trusted event; a human at the keyboard or a pointer can. */
    function isConsidered(event) {
        return event.isTrusted && performance.now() - shownAt >= SETTLE_MS;
    }
    function onClick(event, g, option) {
        if (option.granting && !isConsidered(event))
            return;
        void answer(g, option);
    }
    // Escape denies, and so does Enter when focus is in the panel but on no
    // button. Enter on a button activates that button, as a keyboard user expects.
    panel.addEventListener("keydown", (event) => {
        const onButton = event.target instanceof Element && event.target.closest("button") !== null;
        if (event.key === "Escape" || (event.key === "Enter" && !onButton)) {
            event.preventDefault();
            if (!event.repeat)
                deny();
        }
    });
    // ── drawing ──
    function build(g) {
        shownAt = performance.now();
        options = g.options.map((option) => makeOption(option, (event, o) => onClick(event, g, o)));
        setText(kicker, kickerFor(g));
        content.replaceChildren(el("h2", { id: "gate-title", class: g.kind === "scope" ? "gate-title" : "gate-title mono" }, titleFor(g)), ...bodyFor(g), el("div", { class: "gate-options" }, ...options.map((o) => el("div", { class: "opt" }, o.button, o.hint))));
    }
    function patchOptions(g) {
        g.options.forEach((option, i) => {
            const o = options[i];
            if (!o)
                return;
            o.option = option;
            const disabled = option.granting && !option.allowed; // a deny is never disabled
            if (o.button.disabled !== disabled)
                o.button.disabled = disabled;
            setText(o.hint, option.hint ?? "");
            setHidden(o.hint, option.hint === null);
        });
    }
    function clear() {
        setHidden(panel, true);
        structure = "";
        options = [];
        content.replaceChildren();
        document.title = BASE_TITLE;
        reporter.show(null);
    }
    return {
        update(gates) {
            const top = gates?.top ?? null;
            const previous = gate;
            gate = top;
            if (top === null)
                return clear();
            const next = structureOf(top);
            if (previous === null || next !== structure) {
                structure = next;
                build(top);
            }
            patchOptions(top);
            const waiting = gates?.waiting ?? 1;
            setText(count, waiting > 1 ? `1 of ${waiting} waiting` : "");
            setHidden(panel, false);
            document.title = TITLE_PREFIX + BASE_TITLE;
            reporter.show(top.kind === "tool" ? null : idOf(top));
        },
        resync: reporter.resync,
        destroy() {
            reporter.destroy();
            document.title = BASE_TITLE;
            panel.remove();
        },
    };
}
